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

    pub async fn create_from(client: kube::Client, namespace: &str, spec: &Pod) -> Result<Self> {
        let pods: Api<Pod> = Api::namespaced(client, namespace);
        let name = spec.metadata.name.clone().unwrap_or_default();
        pods.create(&PostParams::default(), spec)
            .await
            .with_context(|| format!("creating pod {name} in {namespace}"))?;
        Ok(Self { pods, name })
    }

    pub fn existing(client: kube::Client, namespace: &str, name: &str) -> Self {
        Self {
            pods: Api::namespaced(client, namespace),
            name: name.to_string(),
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

    async fn finish(&self, attached: AttachedProcess, argv: &[String]) -> Result<()> {
        let code = self.exit_code(attached).await;
        if !matches!(code, Ok(0)) {
            bail!(
                "`{}` failed in pod {}: {}",
                argv.join(" "),
                self.name,
                code.map_or_else(|e| format!("{e:#}"), |c| format!("exit code {c}"))
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
        self.finish(attached, argv).await
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
        let mut stdin = attached
            .stdin()
            .ok_or_else(|| anyhow!("exec stdin stream missing"))?;
        stdin.write_all(data).await.context("writing exec stdin")?;
        stdin.shutdown().await.context("closing exec stdin")?;
        drop(stdin);
        self.finish(attached, argv).await
    }

    pub async fn exec_capture(&self, argv: &[String]) -> Result<String> {
        let params = AttachParams::default().stdout(true).stderr(true);
        let mut attached = self
            .pods
            .exec(&self.name, argv.iter().map(String::as_str), &params)
            .await
            .with_context(|| format!("exec `{}` in pod {}", argv.join(" "), self.name))?;
        let mut stdout = attached
            .stdout()
            .ok_or_else(|| anyhow!("exec stdout stream missing"))?;
        let mut buf = Vec::new();
        tokio::io::copy(&mut stdout, &mut buf)
            .await
            .context("capturing exec stdout")?;
        self.finish(attached, argv).await?;
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
