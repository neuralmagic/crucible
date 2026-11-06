#![allow(clippy::disallowed_macros)]

//! The queue: a build goes in as a Job, and two watches, on the Jobs and on their pods, keep a
//! local view of every build. Status reads that view, and a waiter wakes on the watch's next
//! event. Nothing polls the cluster or execs into a pod.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{Stream, StreamExt};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Pod, Secret};
use kube::api::{Api, DeleteParams, LogParams, PostParams};
use kube::runtime::reflector::{self, ObjectRef, Store, store::Writer};
use kube::runtime::{WatchStreamExt, watcher};
use serde::Serialize;
use tokio::sync::watch;

use crate::context::Registry;
use crate::job::{self, BUILDER, Build, LABEL_BUILD, LABEL_MANAGED_BY, MANAGED_BY, Profile};

/// Where a build is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Waiting for Kueue to admit it.
    Queued,
    Running,
    Succeeded,
    Failed,
}

impl State {
    fn ended(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuildStatus {
    pub build_id: String,
    pub state: State,
    /// The pushed image by digest, once it succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Why it failed, with the end of its log.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    /// When Kueue admitted it and it started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished: Option<String>,
}

pub struct BuildRequest {
    pub context: PathBuf,
    pub dockerfile: String,
    pub build_args: BTreeMap<String, String>,
}

pub struct Queue {
    client: kube::Client,
    profile: Profile,
    registry: Registry,
    jobs: Store<Job>,
    pods: Store<Pod>,
    changes: watch::Receiver<u64>,
}

impl Queue {
    /// Starts the watches and returns once both have listed what exists.
    pub async fn start(
        client: kube::Client,
        profile: Profile,
        registry: Registry,
    ) -> Result<Arc<Self>> {
        let (tx, changes) = watch::channel(0u64);
        let cfg = watcher::Config::default().labels(&format!("{LABEL_MANAGED_BY}={MANAGED_BY}"));
        let (jobs, job_writer) = reflector::store();
        let (pods, pod_writer) = reflector::store();
        let job_api: Api<Job> = Api::namespaced(client.clone(), &profile.namespace);
        let pod_api: Api<Pod> = Api::namespaced(client.clone(), &profile.namespace);
        tokio::spawn(follow(
            job_writer,
            watcher(job_api, cfg.clone()),
            tx.clone(),
        ));
        tokio::spawn(follow(pod_writer, watcher(pod_api, cfg), tx));
        jobs.wait_until_ready().await.context("listing builds")?;
        pods.wait_until_ready()
            .await
            .context("listing build pods")?;
        Ok(Arc::new(Self {
            client,
            profile,
            registry,
            jobs,
            pods,
            changes,
        }))
    }

    /// Pushes the context and enqueues its build; returns the build id.
    pub async fn submit(&self, req: &BuildRequest) -> Result<String> {
        let id = new_id();
        let ctx_ref = self.registry.push_context(&req.context).await?;
        let image = format!("{}:{id}", self.registry.repo);
        let spec = job::job(
            &self.profile,
            &Build {
                id: &id,
                ctx_ref: &ctx_ref,
                image: &image,
                dockerfile: &req.dockerfile,
                build_args: &req.build_args,
            },
        );
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.profile.namespace);
        let created = jobs
            .create(&PostParams::default(), &spec)
            .await
            .with_context(|| format!("creating build {id}"))?;
        let uid = created
            .metadata
            .uid
            .ok_or_else(|| anyhow!("build {id} came back without a uid"))?;
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), &self.profile.namespace);
        let secret = job::secret(&self.profile.namespace, &id, &uid, self.registry.authfile());
        if let Err(e) = secrets.create(&PostParams::default(), &secret).await {
            let _ = jobs.delete(&id, &DeleteParams::background()).await;
            return Err(e).with_context(|| format!("creating build {id}'s credential"));
        }
        Ok(id)
    }

    /// The build's status; with `wait`, once it ends or `wait` passes, whichever comes first.
    pub async fn status(&self, id: &str, wait: Option<Duration>) -> Result<BuildStatus> {
        let until = wait.map(|w| Instant::now() + w);
        let mut changes = self.changes.clone();
        loop {
            changes.borrow_and_update();
            let seen = self.observe(id);
            let left = until.map(|u| u.saturating_duration_since(Instant::now()));
            match (seen, left) {
                (Some(s), _) if s.state.ended() => return Ok(s),
                (Some(s), None) => return Ok(s),
                (Some(s), Some(left)) if left.is_zero() => return Ok(s),
                (None, None) => bail!("no build {id}"),
                (None, Some(left)) if left.is_zero() => bail!("no build {id}"),
                (_, Some(left)) => {
                    if let Ok(Err(_)) = tokio::time::timeout(left, changes.changed()).await {
                        bail!("the build watch stopped");
                    }
                }
            }
        }
    }

    /// The last `tail` lines of the build's log.
    pub async fn logs(&self, id: &str, tail: i64) -> Result<String> {
        let pod = self
            .pod_of(id)
            .ok_or_else(|| anyhow!("build {id} has no pod yet"))?;
        let name = pod.metadata.name.clone().unwrap_or_default();
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.profile.namespace);
        pods.logs(
            &name,
            &LogParams {
                container: Some(BUILDER.to_string()),
                tail_lines: Some(tail),
                ..Default::default()
            },
        )
        .await
        .with_context(|| format!("reading build {id}'s log"))
    }

    pub async fn cancel(&self, id: &str) -> Result<()> {
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.profile.namespace);
        jobs.delete(id, &DeleteParams::background())
            .await
            .with_context(|| format!("cancelling build {id}"))?;
        Ok(())
    }

    fn observe(&self, id: &str) -> Option<BuildStatus> {
        let job = self
            .jobs
            .get(&ObjectRef::new(id).within(&self.profile.namespace))?;
        Some(status_of(
            &job,
            self.pod_of(id).as_deref(),
            &self.registry.repo,
        ))
    }

    fn pod_of(&self, id: &str) -> Option<Arc<Pod>> {
        self.pods
            .state()
            .into_iter()
            .filter(|p| {
                p.metadata
                    .labels
                    .as_ref()
                    .and_then(|l| l.get(LABEL_BUILD))
                    .is_some_and(|b| b == id)
            })
            .max_by_key(|p| p.metadata.creation_timestamp.clone())
    }
}

/// Keeps `writer`'s store current from `stream` and bumps `tx` on every event.
async fn follow<K, S>(writer: Writer<K>, stream: S, tx: watch::Sender<u64>)
where
    K: kube::Resource<DynamicType = ()> + Clone + Send + Sync + 'static,
    S: Stream<Item = Result<watcher::Event<K>, watcher::Error>> + Send,
{
    let events = reflector::reflector(writer, stream).default_backoff();
    futures_util::pin_mut!(events);
    while let Some(event) = events.next().await {
        if let Err(e) = event {
            tracing::warn!("build watch: {e}");
        }
        tx.send_modify(|n| *n += 1);
    }
}

fn status_of(job: &Job, pod: Option<&Pod>, repo: &str) -> BuildStatus {
    let id = job.metadata.name.clone().unwrap_or_default();
    let status = job.status.as_ref();
    let condition = |kind: &str| {
        status
            .and_then(|s| s.conditions.as_ref())
            .and_then(|cs| cs.iter().find(|c| c.type_ == kind && c.status == "True"))
    };
    let terminated = pod
        .and_then(|p| p.status.as_ref())
        .and_then(|s| s.container_statuses.as_ref())
        .and_then(|cs| cs.iter().find(|c| c.name == BUILDER))
        .and_then(|c| c.state.as_ref())
        .and_then(|s| s.terminated.as_ref());
    let message = terminated.and_then(|t| t.message.clone());
    let (state, image, message, finished) = if let Some(done) = condition("Complete") {
        let image = message
            .as_deref()
            .map(str::trim)
            .filter(|m| m.starts_with("sha256:"))
            .map(|digest| format!("{repo}@{digest}"));
        (
            State::Succeeded,
            image,
            None,
            done.last_transition_time.as_ref().map(|t| t.0.to_string()),
        )
    } else if let Some(failed) = condition("Failed") {
        let why = [failed.message.clone(), message]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("\n");
        (
            State::Failed,
            None,
            Some(why),
            failed
                .last_transition_time
                .as_ref()
                .map(|t| t.0.to_string()),
        )
    } else if job.spec.as_ref().and_then(|s| s.suspend) == Some(true) {
        (State::Queued, None, None, None)
    } else {
        (State::Running, None, None, None)
    };
    BuildStatus {
        build_id: id,
        state,
        image,
        message,
        created: job
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|t| t.0.to_string()),
        started: status
            .and_then(|s| s.start_time.as_ref())
            .map(|t| t.0.to_string()),
        finished,
    }
}

fn new_id() -> String {
    use sha2::Digest;
    let seed = format!("{}-{:?}", std::process::id(), std::time::SystemTime::now());
    let hash = sha2::Sha256::digest(seed.as_bytes());
    let hex: String = hash[..6].iter().map(|b| format!("{b:02x}")).collect();
    format!("bq-{hex}")
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::batch::v1::Job;
    use k8s_openapi::api::core::v1::Pod;

    use crate::queue::{State, status_of};

    fn job(spec: serde_json::Value, status: serde_json::Value) -> Job {
        serde_json::from_value(serde_json::json!({
            "metadata": { "name": "bq-1", "creationTimestamp": "2026-11-05T00:00:00Z" },
            "spec": spec,
            "status": status,
        }))
        .unwrap()
    }

    fn pod(message: &str) -> Pod {
        serde_json::from_value(serde_json::json!({
            "metadata": { "name": "bq-1-x" },
            "status": { "containerStatuses": [{
                "name": "builder", "ready": false, "restartCount": 0, "image": "b", "imageID": "",
                "state": { "terminated": { "exitCode": 0, "message": message } }
            }]}
        }))
        .unwrap()
    }

    #[test]
    fn a_builds_state_follows_kueue_and_its_job() {
        let template = serde_json::json!({ "spec": { "containers": [] } });
        let suspended = job(
            serde_json::json!({ "suspend": true, "template": template }),
            serde_json::json!({}),
        );
        assert_eq!(status_of(&suspended, None, "r").state, State::Queued);

        let admitted = job(
            serde_json::json!({ "suspend": false, "template": template }),
            serde_json::json!({ "startTime": "2026-11-05T00:01:00Z", "active": 1 }),
        );
        let running = status_of(&admitted, None, "r");
        assert_eq!(running.state, State::Running);
        assert_eq!(running.started.as_deref(), Some("2026-11-05T00:01:00Z"));

        let complete = job(
            serde_json::json!({ "template": template }),
            serde_json::json!({ "conditions": [
                { "type": "Complete", "status": "True", "lastTransitionTime": "2026-11-05T00:05:00Z" }
            ]}),
        );
        let done = status_of(&complete, Some(&pod("sha256:abc\n")), "quay.io/acme/b");
        assert_eq!(done.state, State::Succeeded);
        assert_eq!(done.image.as_deref(), Some("quay.io/acme/b@sha256:abc"));
        assert_eq!(done.finished.as_deref(), Some("2026-11-05T00:05:00Z"));

        let failed = job(
            serde_json::json!({ "template": template }),
            serde_json::json!({ "conditions": [
                { "type": "Failed", "status": "True", "message": "BackoffLimitExceeded" }
            ]}),
        );
        let broke = status_of(&failed, Some(&pod("error: no such file")), "r");
        assert_eq!(broke.state, State::Failed);
        assert_eq!(
            broke.message.as_deref(),
            Some("BackoffLimitExceeded\nerror: no such file")
        );
        assert_eq!(broke.image, None);
    }
}
