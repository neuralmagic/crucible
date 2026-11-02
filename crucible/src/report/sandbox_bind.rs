//! Binding a sandbox the run pod created to its run at the controller, which issues agent tokens
//! only to bound sandboxes. The run's pod token authenticates it, as it does the run's evidence.

use std::time::Duration;

const MAX_ATTEMPTS: u32 = 3;
const BACKOFF: Duration = Duration::from_secs(1);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Where this pod binds its sandboxes: the controller, the run, and the run's pod token.
pub(crate) struct BindTarget {
    base: String,
    run: String,
    token: String,
}

impl BindTarget {
    /// Present only when the controller injected its URL, the run's id, and the pod token.
    pub(crate) fn from_env() -> Option<Self> {
        Some(Self::new(
            non_empty_env(crucible_contract::ENV_CONTROLLER_URL)?,
            non_empty_env(crucible_contract::ENV_RUN_NAME)?,
            non_empty_env(crucible_contract::ENV_POD_TOKEN)?,
        ))
    }

    pub(crate) fn new(
        base: impl Into<String>,
        run: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            run: run.into(),
            token: token.into(),
        }
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum BindError {
    #[error("the gateway named no id for sandbox {0}, so it cannot be bound")]
    NoSandboxId(String),
    #[error("building the sandbox bind client: {0}")]
    Client(#[source] reqwest::Error),
    #[error(
        "binding sandbox {sandbox_id} to run {run} failed after {MAX_ATTEMPTS} attempts: {last}"
    )]
    Refused {
        sandbox_id: String,
        run: String,
        last: String,
    },
}

/// Bind `binding`'s sandbox to the run, retrying a failure; the error names the last one.
pub(crate) async fn bind_sandbox(
    target: &BindTarget,
    binding: &crucible_contract::sandbox::SandboxBinding,
) -> Result<(), BindError> {
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(BindError::Client)?;
    let url = format!(
        "{}{}",
        target.base,
        crucible_contract::sandbox::binding_path(&target.run)
    );
    let mut last = String::new();
    for attempt in 1..=MAX_ATTEMPTS {
        match client
            .post(&url)
            .bearer_auth(&target.token)
            .json(binding)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => return Ok(()),
            Ok(r) => {
                let status = r.status();
                last = format!(
                    "HTTP {status}: {}",
                    r.text().await.unwrap_or_default().trim()
                );
            }
            Err(e) => last = e.to_string(),
        }
        if attempt < MAX_ATTEMPTS {
            tokio::time::sleep(BACKOFF * attempt).await;
        }
    }
    Err(BindError::Refused {
        sandbox_id: binding.sandbox_id.clone(),
        run: target.run.clone(),
        last,
    })
}

#[cfg(test)]
mod tests {
    use crate::report::sandbox_bind::*;

    /// A one-route stand-in for the controller's bind endpoint on loopback: answers every request
    /// with `status` and hands back what it received.
    async fn bind_endpoint(
        status: &'static str,
    ) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let (mut conn, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let request = loop {
                    let n = conn.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let len = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if body.len() >= len || n == 0 {
                            break text;
                        }
                    }
                };
                tx.send(request).unwrap();
                let reply =
                    format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
                conn.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        (format!("http://{addr}"), rx)
    }

    fn binding() -> crucible_contract::sandbox::SandboxBinding {
        crucible_contract::sandbox::SandboxBinding {
            sandbox_id: "sbx-1".into(),
            workdir: Some("/sandbox/repo".into()),
        }
    }

    #[tokio::test]
    async fn binding_posts_the_sandbox_id_to_the_run_with_its_pod_token() {
        let (base, mut seen) = bind_endpoint("204 No Content").await;
        let target = BindTarget::new(format!("{base}/"), "playbook_x_01/a", "crr_id_secret");
        bind_sandbox(&target, &binding()).await.unwrap();
        let request = seen.recv().await.unwrap();
        assert!(
            request.starts_with("POST /api/sandbox-bindings/runs/playbook_x_01%2Fa HTTP/1.1"),
            "{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer crr_id_secret\r\n"),
            "{request}"
        );
        assert!(
            request.ends_with(r#"{"sandbox_id":"sbx-1","workdir":"/sandbox/repo"}"#),
            "{request}"
        );
    }

    #[tokio::test]
    async fn a_refused_bind_is_retried_then_reported() {
        let (base, mut seen) = bind_endpoint("409 Conflict").await;
        let target = BindTarget::new(base, "run-a", "crr_id_secret");
        let err = bind_sandbox(&target, &binding()).await.unwrap_err();
        assert!(err.to_string().contains("HTTP 409"), "{err:#}");
        assert!(err.to_string().contains("run run-a"), "{err:#}");
        for _ in 0..MAX_ATTEMPTS {
            seen.recv().await.unwrap();
        }
    }
}
