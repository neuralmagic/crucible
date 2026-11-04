#![allow(clippy::disallowed_macros)]

use std::hash::{BuildHasher, RandomState};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{
    Api, AttachParams, AttachedProcess, DeleteParams, ListParams, LogParams, PostParams,
};
use kube::runtime::wait::{await_condition, conditions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::backend::Backend;

pub struct ExecOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    // None when stdout went past the limit and the process was cut off
    pub code: Option<i32>,
}

pub struct BuilderPod {
    pods: Api<Pod>,
    pub name: String,
}

/// An exec's stderr, read as it arrives: kube buffers each stream in 1 KiB, and an unread stream
/// stalls the others and the exit status behind it.
struct Stderr(Option<tokio::task::JoinHandle<Vec<u8>>>);

const STDERR_TAIL: usize = 2048;

impl Stderr {
    fn drain(attached: &mut AttachedProcess) -> Self {
        Self(attached.stderr().map(|mut err| {
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let _ = err.read_to_end(&mut buf).await;
                buf
            })
        }))
    }

    /// The end of what the command wrote to stderr, as `: <text>`, or nothing.
    async fn collect(self) -> String {
        let Some(task) = self.0 else {
            return String::new();
        };
        let buf = task.await.unwrap_or_default();
        let text = String::from_utf8_lossy(&buf);
        let text = text.trim();
        if text.is_empty() {
            return String::new();
        }
        let start = text.floor_char_boundary(text.len().saturating_sub(STDERR_TAIL));
        format!(": {}", &text[start..])
    }
}

/// The scheduling gate Kueue holds a pod behind until it admits the pod's workload.
pub const KUEUE_GATE: &str = "kueue.x-k8s.io/admission";

const CREATE_TRIES: u32 = 4;
const CREATE_BACKOFF: Duration = Duration::from_secs(2);

/// An error the API server may not repeat: a 5xx or 429, such as an admission plugin that timed
/// out, or a connection that broke.
pub(crate) fn transient(e: &kube::Error) -> bool {
    match e {
        kube::Error::Api(status) => status.code >= 500 || status.code == 429,
        kube::Error::HyperError(_) | kube::Error::Service(_) => true,
        _ => false,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Release {
    Released,
    /// Still gated when the wait ended, by these gates.
    Held(Vec<String>),
}

fn scheduling_gates(pod: &Pod) -> Vec<&str> {
    pod.spec
        .as_ref()
        .and_then(|s| s.scheduling_gates.as_deref())
        .unwrap_or_default()
        .iter()
        .map(|g| g.name.as_str())
        .collect()
}

pub(crate) fn unique_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("buildit-{:016x}", RandomState::new().hash_one(nanos))
}

impl BuilderPod {
    pub async fn create(
        client: kube::Client,
        namespace: &str,
        backend: Backend,
        opts: &crate::backend::PodOpts<'_>,
    ) -> Result<Self> {
        let spec = backend.pod_spec(&unique_name(), namespace, opts)?;
        Self::create_from(client, namespace, &spec).await
    }

    /// Create the pod, again after a transient error; a conflict on a retry is an earlier try
    /// that landed after all.
    pub async fn create_from(client: kube::Client, namespace: &str, spec: &Pod) -> Result<Self> {
        let pods: Api<Pod> = Api::namespaced(client, namespace);
        let name = spec.metadata.name.clone().unwrap_or_default();
        let mut backoff = CREATE_BACKOFF;
        for attempt in 1..=CREATE_TRIES {
            match pods.create(&PostParams::default(), spec).await {
                Ok(_) => break,
                Err(kube::Error::Api(status)) if attempt > 1 && status.code == 409 => break,
                Err(e) if attempt < CREATE_TRIES && transient(&e) => {
                    tracing::warn!("creating pod {name} (try {attempt}): {e}; retrying");
                    tokio::time::sleep(backoff).await;
                    backoff *= 2;
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("creating pod {name} in {namespace}"));
                }
            }
        }
        Ok(Self { pods, name })
    }

    pub fn existing(client: kube::Client, namespace: &str, name: &str) -> Self {
        Self {
            pods: Api::namespaced(client, namespace),
            name: name.to_string(),
        }
    }

    /// Wait until no scheduling gate holds the pod, as Kueue holds one it has not admitted.
    pub async fn wait_ungated(&self, timeout: Duration) -> Result<Release> {
        let ungated = |p: Option<&Pod>| p.is_none_or(|p| scheduling_gates(p).is_empty());
        let watched = tokio::time::timeout(
            timeout,
            await_condition(self.pods.clone(), &self.name, ungated),
        )
        .await;
        match watched {
            Ok(Ok(Some(_))) => Ok(Release::Released),
            Ok(Ok(None)) => bail!("pod {} was deleted before it was scheduled", self.name),
            Ok(Err(e)) => Err(e).with_context(|| format!("waiting for pod {}", self.name)),
            Err(_) => {
                let pod = self
                    .pods
                    .get(&self.name)
                    .await
                    .with_context(|| format!("reading pod {}", self.name))?;
                Ok(Release::Held(
                    scheduling_gates(&pod)
                        .into_iter()
                        .map(String::from)
                        .collect(),
                ))
            }
        }
    }

    pub async fn wait_ready(&self, timeout: Duration) -> Result<()> {
        tokio::time::timeout(
            timeout,
            await_condition(self.pods.clone(), &self.name, conditions::is_pod_running()),
        )
        .await
        .map_err(|_| anyhow!("pod {} not running after {timeout:?}", self.name))?
        .with_context(|| format!("waiting for pod {}", self.name))?;
        Ok(())
    }

    // Err when the command never ran
    async fn exit_code(&self, mut attached: AttachedProcess) -> Result<i32> {
        let status = attached
            .take_status()
            .ok_or_else(|| anyhow!("exec status channel already taken"))?
            .await;
        attached.join().await.context("joining exec stream")?;
        let status =
            status.ok_or_else(|| anyhow!("no exit status received from pod {}", self.name))?;
        if status.status.as_deref() == Some("Success") {
            return Ok(0);
        }
        status
            .details
            .as_ref()
            .and_then(|d| d.causes.as_deref())
            .unwrap_or_default()
            .iter()
            .find(|c| c.reason.as_deref() == Some("ExitCode"))
            .and_then(|c| c.message.as_deref()?.trim().parse().ok())
            .ok_or_else(|| anyhow!("{}", status.message.unwrap_or_default()))
    }

    async fn finish(
        &self,
        attached: AttachedProcess,
        stderr: Stderr,
        argv: &[String],
    ) -> Result<()> {
        let code = self.exit_code(attached).await;
        let stderr = stderr.collect().await;
        if !matches!(code, Ok(0)) {
            bail!(
                "`{}` failed in pod {}: {}{}",
                argv.join(" "),
                self.name,
                code.map_or_else(|e| format!("{e:#}"), |c| format!("exit code {c}")),
                stderr
            );
        }
        Ok(())
    }

    // exit status comes from the real status frame; a piped exit code once
    // lied about a segfault and that's why this tool exists
    pub async fn exec_stream(&self, argv: &[String]) -> Result<()> {
        let params = AttachParams::default().stdout(true).stderr(true);
        let mut attached = self
            .pods
            .exec(&self.name, argv.iter().map(String::as_str), &params)
            .await
            .with_context(|| format!("exec `{}` in pod {}", argv.join(" "), self.name))?;
        let mut stdout = attached
            .stdout()
            .ok_or_else(|| anyhow!("exec stdout stream missing"))?;
        let mut stderr = attached
            .stderr()
            .ok_or_else(|| anyhow!("exec stderr stream missing"))?;
        let mut our_stdout = tokio::io::stdout();
        let mut our_stderr = tokio::io::stderr();
        let out = tokio::io::copy(&mut stdout, &mut our_stdout);
        let err = tokio::io::copy(&mut stderr, &mut our_stderr);
        let (out, err) = tokio::join!(out, err);
        out.context("streaming exec stdout")?;
        err.context("streaming exec stderr")?;
        self.finish(attached, Stderr(None), argv).await
    }

    pub async fn exec_with_stdin(&self, argv: &[String], data: &[u8]) -> Result<()> {
        let params = AttachParams::default()
            .stdin(true)
            .stdout(true)
            .stderr(true);
        let mut attached = self
            .pods
            .exec(&self.name, argv.iter().map(String::as_str), &params)
            .await
            .with_context(|| format!("exec `{}` in pod {}", argv.join(" "), self.name))?;
        let stderr = Stderr::drain(&mut attached);
        let stdout = attached.stdout().map(|mut out| {
            tokio::spawn(async move { tokio::io::copy(&mut out, &mut tokio::io::sink()).await })
        });
        let mut stdin = attached
            .stdin()
            .ok_or_else(|| anyhow!("exec stdin stream missing"))?;
        stdin.write_all(data).await.context("writing exec stdin")?;
        stdin.shutdown().await.context("closing exec stdin")?;
        drop(stdin);
        if let Some(out) = stdout {
            out.await
                .context("joining exec stdout")?
                .context("reading exec stdout")?;
        }
        self.finish(attached, stderr, argv).await
    }

    pub async fn exec_with_stdin_capture(&self, argv: &[String], data: &[u8]) -> Result<String> {
        let params = AttachParams::default()
            .stdin(true)
            .stdout(true)
            .stderr(true);
        let mut attached = self
            .pods
            .exec(&self.name, argv.iter().map(String::as_str), &params)
            .await
            .with_context(|| format!("exec `{}` in pod {}", argv.join(" "), self.name))?;
        let stderr = Stderr::drain(&mut attached);
        let mut stdout = attached
            .stdout()
            .ok_or_else(|| anyhow!("exec stdout stream missing"))?;
        let captured = tokio::spawn(async move {
            let mut buf = Vec::new();
            tokio::io::copy(&mut stdout, &mut buf).await.map(|_| buf)
        });
        let mut stdin = attached
            .stdin()
            .ok_or_else(|| anyhow!("exec stdin stream missing"))?;
        stdin.write_all(data).await.context("writing exec stdin")?;
        stdin.shutdown().await.context("closing exec stdin")?;
        drop(stdin);
        let buf = captured
            .await
            .context("joining exec stdout")?
            .context("capturing exec stdout")?;
        self.finish(attached, stderr, argv).await?;
        String::from_utf8(buf).context("exec output was not utf-8")
    }

    pub async fn exec_capture(&self, argv: &[String]) -> Result<String> {
        let params = AttachParams::default().stdout(true).stderr(true);
        let mut attached = self
            .pods
            .exec(&self.name, argv.iter().map(String::as_str), &params)
            .await
            .with_context(|| format!("exec `{}` in pod {}", argv.join(" "), self.name))?;
        let stderr = Stderr::drain(&mut attached);
        let mut stdout = attached
            .stdout()
            .ok_or_else(|| anyhow!("exec stdout stream missing"))?;
        let mut buf = Vec::new();
        tokio::io::copy(&mut stdout, &mut buf)
            .await
            .context("capturing exec stdout")?;
        self.finish(attached, stderr, argv).await?;
        String::from_utf8(buf).context("exec output was not utf-8")
    }

    // stdout up to `limit` bytes, stderr, and the exit status
    pub async fn exec_output(&self, argv: &[String], limit: u64) -> Result<ExecOutput> {
        let params = AttachParams::default().stdout(true).stderr(true);
        let mut attached = self
            .pods
            .exec(&self.name, argv.iter().map(String::as_str), &params)
            .await
            .with_context(|| format!("exec `{}` in pod {}", argv.join(" "), self.name))?;
        let stdout = attached
            .stdout()
            .ok_or_else(|| anyhow!("exec stdout stream missing"))?;
        let mut stderr = attached
            .stderr()
            .ok_or_else(|| anyhow!("exec stderr stream missing"))?;
        let err = tokio::spawn(async move {
            let mut buf = Vec::new();
            stderr.read_to_end(&mut buf).await.map(|_| buf)
        });
        let mut out = Vec::new();
        stdout
            .take(limit.saturating_add(1))
            .read_to_end(&mut out)
            .await
            .context("reading exec stdout")?;
        if out.len() as u64 > limit {
            err.abort();
            out.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
            return Ok(ExecOutput {
                stdout: out,
                stderr: Vec::new(),
                code: None,
            });
        }
        let stderr = err
            .await
            .context("joining exec stderr")?
            .context("reading exec stderr")?;
        Ok(ExecOutput {
            stdout: out,
            stderr,
            code: Some(self.exit_code(attached).await?),
        })
    }

    pub async fn container_log(&self) -> Result<String> {
        self.pods
            .logs(&self.name, &LogParams::default())
            .await
            .with_context(|| format!("reading the logs of pod {}", self.name))
    }

    pub async fn delete(&self) -> Result<()> {
        self.pods
            .delete(&self.name, &DeleteParams::default())
            .await
            .with_context(|| format!("deleting pod {}", self.name))?;
        Ok(())
    }
}

// get-or-create the cache PVC; existing claims are used as-is
pub async fn ensure_pvc(
    client: kube::Client,
    namespace: &str,
    name: &str,
    size: &str,
) -> Result<()> {
    use k8s_openapi::api::core::v1::PersistentVolumeClaim;
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client, namespace);
    if pvcs
        .get_opt(name)
        .await
        .context("checking cache PVC")?
        .is_some()
    {
        return Ok(());
    }
    let claim: PersistentVolumeClaim = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "labels": { "app": "buildit", "buildit/cache": "true" }
        },
        "spec": {
            "accessModes": ["ReadWriteOnce"],
            "resources": { "requests": { "storage": size } }
        }
    }))
    .context("building PVC spec")?;
    pvcs.create(&PostParams::default(), &claim)
        .await
        .with_context(|| format!("creating cache PVC {name}"))?;
    tracing::info!("created cache PVC {name} ({size})");
    Ok(())
}

pub async fn clean(client: kube::Client, namespace: &str) -> Result<usize> {
    let pods: Api<Pod> = Api::namespaced(client, namespace);
    let list = pods
        .list(&ListParams::default().labels("app=buildit"))
        .await
        .with_context(|| format!("listing buildit pods in {namespace}"))?;
    let mut deleted = 0;
    for pod in list.items {
        // job pods carry app=buildit too but belong to their Job; the job
        // sweep handles those via cascade
        if !pod
            .metadata
            .owner_references
            .as_deref()
            .unwrap_or_default()
            .is_empty()
        {
            continue;
        }
        if let Some(name) = pod.metadata.name {
            pods.delete(&name, &DeleteParams::default())
                .await
                .with_context(|| format!("deleting pod {name}"))?;
            tracing::info!("deleted pod {name}");
            deleted += 1;
        }
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use crate::pod::transient;

    fn api(code: u16) -> kube::Error {
        let status: kube::core::Status = serde_json::from_value(serde_json::json!({
            "status": "Failure",
            "code": code,
            "message": "admission plugin \"LimitRanger\" failed to complete mutation in 13s",
        }))
        .unwrap();
        kube::Error::Api(Box::new(status))
    }

    #[test]
    fn server_errors_and_throttling_are_retried_and_refusals_are_not() {
        for code in [500, 502, 503, 504, 429] {
            assert!(transient(&api(code)), "{code}");
        }
        for code in [400, 403, 404, 409, 422] {
            assert!(!transient(&api(code)), "{code}");
        }
        let bad_json = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        assert!(!transient(&kube::Error::SerdeError(bad_json)));
    }
}
