#![allow(clippy::disallowed_macros)]

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::TryStreamExt;
use futures_util::io::AsyncBufReadExt;
use k8s_openapi::ByteString;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Pod, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{Api, DeleteParams, ListParams, LogParams, ObjectMeta, PostParams};

use crate::backend::Backend;
use crate::build::pinned_ref;
use crate::{auth, oci, pod};

pub struct DetachArgs<'a> {
    pub image: &'a str,
    pub dockerfile: &'a str,
    pub target: Option<&'a str>,
    pub build_args: &'a [String],
    pub ctx_ref: &'a str,
    pub authfile: &'a [u8],
    pub idle_nodes: &'a [String],
    pub resources: &'a crate::backend::Resources,
    pub labels: &'a [(String, String)],
    pub cache: Option<crate::backend::CacheVolume<'a>>,
    pub node: Option<&'a str>,
}

pub fn secret_manifest(
    name: &str,
    namespace: &str,
    authfile: &[u8],
    owner: Option<OwnerReference>,
) -> Secret {
    Secret {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(BTreeMap::from([("app".to_string(), "buildit".to_string())])),
            owner_references: owner.map(|o| vec![o]),
            ..Default::default()
        },
        type_: Some("kubernetes.io/dockerconfigjson".to_string()),
        data: Some(BTreeMap::from([(
            ".dockerconfigjson".to_string(),
            ByteString(authfile.to_vec()),
        )])),
        ..Default::default()
    }
}

// job first, then the secret owner-ref'd to it: when ttlSecondsAfterFinished
// reaps the job, the secret cascades away with it
pub async fn create(
    client: kube::Client,
    namespace: &str,
    backend: Backend,
    args: &DetachArgs<'_>,
) -> Result<String> {
    let jobs: Api<Job> = Api::namespaced(client.clone(), namespace);
    let secrets: Api<Secret> = Api::namespaced(client, namespace);
    let name = pod::unique_name();

    let spec = backend.job_spec(&name, namespace, args)?;
    let created = jobs
        .create(&PostParams::default(), &spec)
        .await
        .with_context(|| format!("creating job {name} in {namespace}"))?;
    let uid = created
        .metadata
        .uid
        .ok_or_else(|| anyhow!("created job has no uid"))?;

    let owner = OwnerReference {
        api_version: "batch/v1".to_string(),
        kind: "Job".to_string(),
        name: name.clone(),
        uid,
        ..Default::default()
    };
    let secret = secret_manifest(&name, namespace, args.authfile, Some(owner));
    if let Err(err) = secrets.create(&PostParams::default(), &secret).await {
        jobs.delete(&name, &DeleteParams::background()).await.ok();
        return Err(err).with_context(|| format!("creating auth secret {name}"));
    }
    Ok(name)
}

enum JobState {
    Running,
    Complete,
    Failed(String),
}

fn job_state(job: &Job) -> JobState {
    let conditions = job
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_deref())
        .unwrap_or_default();
    for c in conditions {
        if c.status != "True" {
            continue;
        }
        match c.type_.as_str() {
            "Complete" => return JobState::Complete,
            "Failed" => {
                return JobState::Failed(
                    c.message
                        .clone()
                        .unwrap_or_else(|| c.reason.clone().unwrap_or_default()),
                );
            }
            _ => {}
        }
    }
    JobState::Running
}

// waits for the job, following builder logs across retries, then prints the
// digest-pinned ref. safe to kill and rerun; it drives nothing.
pub async fn wait(client: kube::Client, namespace: &str, job_name: &str) -> Result<()> {
    let jobs: Api<Job> = Api::namespaced(client.clone(), namespace);
    let pods: Api<Pod> = Api::namespaced(client, namespace);
    let mut streamed: HashSet<String> = HashSet::new();

    let job = jobs
        .get(job_name)
        .await
        .with_context(|| format!("getting job {job_name} (TTL may have reaped it)"))?;
    let backend = backend_of(&job)?;
    let image = job
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get("buildit/image").cloned())
        .ok_or_else(|| anyhow!("job {job_name} has no buildit/image annotation"))?;

    loop {
        let job = jobs.get(job_name).await.context("polling job")?;
        match job_state(&job) {
            JobState::Complete => break,
            JobState::Failed(msg) => {
                dump_failure_message(&pods, job_name).await;
                bail!("job {job_name} failed: {msg}");
            }
            JobState::Running => {}
        }
        follow_new_pod_logs(&pods, job_name, &mut streamed).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    let digest = match termination_message(&pods, job_name).await {
        Some(msg) => backend.digest_from(&msg)?,
        None => {
            tracing::warn!("no termination message found, resolving digest from registry");
            let registry = auth::registry_of(&image)?;
            let (user, pass) = auth::basic_credentials(&registry)?;
            let auth = oci_client::secrets::RegistryAuth::Basic(user, pass);
            oci::resolve_digest(&image, &auth).await?
        }
    };
    tracing::info!("pushed {image}");
    println!("{}", pinned_ref(&image, &digest));
    Ok(())
}

fn backend_of(job: &Job) -> Result<Backend> {
    let label = job
        .metadata
        .labels
        .as_ref()
        .and_then(|l| l.get("buildit/backend").cloned())
        .ok_or_else(|| anyhow!("job has no buildit/backend label"))?;
    match label.as_str() {
        "buildkit" => Ok(Backend::Buildkit),
        "kaniko" => Ok(Backend::Kaniko),
        "buildah" => Ok(Backend::Buildah),
        other => Err(anyhow!("unknown backend label {other:?}")),
    }
}

async fn job_pods(pods: &Api<Pod>, job_name: &str) -> Vec<Pod> {
    pods.list(&ListParams::default().labels(&format!("job-name={job_name}")))
        .await
        .map(|l| l.items)
        .unwrap_or_default()
}

// each pod's logs get followed exactly once, so a retry pod picks up cleanly
async fn follow_new_pod_logs(pods: &Api<Pod>, job_name: &str, streamed: &mut HashSet<String>) {
    for p in job_pods(pods, job_name).await {
        let Some(name) = p.metadata.name else {
            continue;
        };
        if streamed.contains(&name) {
            continue;
        }
        let params = LogParams {
            container: Some("builder".to_string()),
            follow: true,
            ..Default::default()
        };
        let Ok(stream) = pods.log_stream(&name, &params).await else {
            continue; // builder not started yet, retry next tick
        };
        streamed.insert(name.clone());
        tracing::info!("following logs of pod {name}");
        let mut lines = stream.lines();
        while let Ok(Some(line)) = lines.try_next().await {
            println!("{line}");
        }
    }
}

async fn termination_message(pods: &Api<Pod>, job_name: &str) -> Option<String> {
    let mut all = job_pods(pods, job_name).await;
    all.sort_by_key(|p| p.metadata.creation_timestamp.clone());
    for p in all.iter().rev() {
        let msg = p
            .status
            .as_ref()?
            .container_statuses
            .as_ref()?
            .iter()
            .find(|c| c.name == "builder")?
            .state
            .as_ref()?
            .terminated
            .as_ref()
            .filter(|t| t.exit_code == 0)
            .and_then(|t| t.message.clone());
        if msg.is_some() {
            return msg;
        }
    }
    None
}

// on failure the termination message carries the tail of the build log
// (terminationMessagePolicy: FallbackToLogsOnError), so show it
async fn dump_failure_message(pods: &Api<Pod>, job_name: &str) {
    for p in job_pods(pods, job_name).await {
        let msg = p
            .status
            .as_ref()
            .and_then(|s| s.container_statuses.as_ref())
            .and_then(|cs| cs.iter().find(|c| c.name == "builder"))
            .and_then(|c| c.state.as_ref())
            .and_then(|s| s.terminated.as_ref())
            .and_then(|t| t.message.as_deref());
        if let Some(msg) = msg {
            tracing::error!(
                "pod {}: {}",
                p.metadata.name.as_deref().unwrap_or("?"),
                msg.trim_end()
            );
        }
    }
}

// jobs cascade to their pods and (via ownerRef) their secrets
pub async fn clean(client: kube::Client, namespace: &str) -> Result<usize> {
    let jobs: Api<Job> = Api::namespaced(client.clone(), namespace);
    let secrets: Api<Secret> = Api::namespaced(client, namespace);
    let mut deleted = 0;
    let list = jobs
        .list(&ListParams::default().labels("app=buildit"))
        .await
        .with_context(|| format!("listing buildit jobs in {namespace}"))?;
    for job in list.items {
        if let Some(name) = job.metadata.name {
            jobs.delete(&name, &DeleteParams::background())
                .await
                .with_context(|| format!("deleting job {name}"))?;
            tracing::info!("deleted job {name}");
            deleted += 1;
        }
    }
    // orphaned secrets (job create succeeded, ownerRef write raced a delete)
    let list = secrets
        .list(&ListParams::default().labels("app=buildit"))
        .await
        .with_context(|| format!("listing buildit secrets in {namespace}"))?;
    for secret in list.items {
        if secret
            .metadata
            .owner_references
            .as_deref()
            .unwrap_or_default()
            .is_empty()
            && let Some(name) = secret.metadata.name
        {
            secrets
                .delete(&name, &DeleteParams::default())
                .await
                .with_context(|| format!("deleting secret {name}"))?;
            tracing::info!("deleted orphaned secret {name}");
            deleted += 1;
        }
    }
    Ok(deleted)
}
