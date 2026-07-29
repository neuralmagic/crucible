//! Typed Kubernetes operations: the engine talks to the API server through `kube-rs` instead of
//! shelling `kubectl`. One place owns the client, the patches, the rollout-wait, pod exec, GPU
//! headroom, and server-side apply; forge's `deploy`, the broker's GPU admission, the deploy-render
//! `apply`, and the domain hooks (via the `forge-kube` CLI) all route through here.
//!
//! Sync surface over an async client: every public fn is blocking so it drops into forge's otherwise-sync
//! code and the broker's `spawn_blocking` paths. [`block_on`] detects an ambient multi-thread runtime
//! (the broker) and uses `block_in_place` there, else spins a private current-thread runtime (the CLI /
//! forge bins), so the same fn is safe whether or not it's called from inside tokio.

use anyhow::{Context, Result, bail};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ConfigMap, Namespace, Node, Pod, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::Client;
use kube::api::{
    Api, AttachParams, DeleteParams, ListParams, LogParams, Patch, PatchParams, PostParams,
};
use kube::core::{DynamicObject, GroupVersionKind, TypeMeta};
use kube::discovery::{Discovery, Scope};
use std::future::Future;
use std::time::{Duration, Instant};

/// Run a future to completion from sync code, whether or not a tokio runtime is already running. Inside
/// the broker's multi-thread runtime we `block_in_place` + reuse the current handle; from a plain sync
/// caller (a forge bin, the CLI) we build a private current-thread runtime.
fn block_on<F: Future>(fut: F) -> Result<F::Output> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => Ok(tokio::task::block_in_place(|| handle.block_on(fut))),
        Err(_) => Ok(tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("building a kube runtime")?
            .block_on(fut)),
    }
}

/// A connected client (in-cluster SA token when running in a pod, else the kubeconfig `kube-rs` resolves
/// from `$KUBECONFIG`/`~/.kube/config`, the same precedence `kubectl` uses).
async fn client() -> Result<Client> {
    // rustls 0.23 won't auto-pick a CryptoProvider when several are linked (aws-lc-rs is, via aws-sdk),
    // so choose one before the TLS handshake. Idempotent: a second install just returns Err, ignored.
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
    Client::try_default()
        .await
        .context("connecting to the Kubernetes API (in-cluster SA or kubeconfig)")
}

/// `kubectl set image deploy/<name> <container>=<image>`: a strategic-merge patch on the one container.
pub fn set_image(ns: &str, deployment: &str, container: &str, image: &str) -> Result<()> {
    block_on(async {
        let api = deployments(client().await?, ns);
        let patch = serde_json::json!({
            "spec": { "template": { "spec": { "containers": [
                { "name": container, "image": image }
            ] } } }
        });
        api.patch(
            deployment,
            &PatchParams::default(),
            &Patch::Strategic(patch),
        )
        .await
        .with_context(|| format!("set image {container}={image} on deploy/{deployment}"))?;
        Ok(())
    })?
}

/// Add an `imagePullSecrets` entry to a deployment's pod template (for private candidate images).
pub fn set_pull_secret(ns: &str, deployment: &str, secret: &str) -> Result<()> {
    block_on(async {
        let api = deployments(client().await?, ns);
        let patch = serde_json::json!({
            "spec": { "template": { "spec": { "imagePullSecrets": [ { "name": secret } ] } } }
        });
        api.patch(
            deployment,
            &PatchParams::default(),
            &Patch::Strategic(patch),
        )
        .await
        .with_context(|| format!("set imagePullSecrets={secret} on deploy/{deployment}"))?;
        Ok(())
    })?
}

/// Set a deployment's RollingUpdate surge/unavailable (kill-old-first on a GPU-saturated node uses
/// `max_surge=0, max_unavailable=1` so the default surge can't deadlock).
pub fn set_rolling_update(
    ns: &str,
    deployment: &str,
    max_surge: i32,
    max_unavailable: i32,
) -> Result<()> {
    block_on(async {
        let api = deployments(client().await?, ns);
        let patch = serde_json::json!({
            "spec": { "strategy": { "type": "RollingUpdate", "rollingUpdate": {
                "maxSurge": max_surge, "maxUnavailable": max_unavailable
            } } }
        });
        api.patch(
            deployment,
            &PatchParams::default(),
            &Patch::Strategic(patch),
        )
        .await
        .with_context(|| format!("set rollingUpdate on deploy/{deployment}"))?;
        Ok(())
    })?
}

/// `kubectl rollout restart`: bump a pod-template annotation so even an unchanged (mutable) image tag
/// re-pulls. Uses a fixed marker value; the rollout controller treats any template change as a new roll.
pub fn rollout_restart(ns: &str, deployment: &str) -> Result<()> {
    block_on(async {
        let api = deployments(client().await?, ns);
        let patch = serde_json::json!({
            "spec": { "template": { "metadata": { "annotations": {
                "forge.crucible/restartedAt": rfc3339_nowish()
            } } } }
        });
        api.patch(
            deployment,
            &PatchParams::default(),
            &Patch::Strategic(patch),
        )
        .await
        .with_context(|| format!("rollout restart deploy/{deployment}"))?;
        Ok(())
    })?
}

/// `kubectl rollout status --timeout=<t>`: poll until the deployment's updated+available replicas reach
/// the spec and the latest generation is observed. Errors on timeout.
pub fn rollout_status(ns: &str, deployment: &str, timeout: Duration) -> Result<()> {
    block_on(async {
        let api = deployments(client().await?, ns);
        let deadline = Instant::now() + timeout;
        loop {
            let d = api
                .get(deployment)
                .await
                .with_context(|| format!("getting deploy/{deployment} for rollout status"))?;
            if rollout_complete(&d) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!(
                    "deploy/{deployment} did not complete its rollout within {}s",
                    timeout.as_secs()
                );
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    })?
}

/// True when the deployment's controller has observed the current spec and all desired replicas are
/// updated and available, the steady state `kubectl rollout status` waits for.
fn rollout_complete(d: &Deployment) -> bool {
    let desired = d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
    let Some(st) = &d.status else { return false };
    let observed = st.observed_generation.unwrap_or(0);
    let generation = d.metadata.generation.unwrap_or(0);
    observed >= generation
        && st.updated_replicas.unwrap_or(0) >= desired
        && st.available_replicas.unwrap_or(0) >= desired
        && st.unavailable_replicas.unwrap_or(0) == 0
}

/// The image of `container` in `deployment`, or `None` if the deployment/container isn't found.
pub fn current_image(ns: &str, deployment: &str, container: &str) -> Result<Option<String>> {
    block_on(async {
        let api = deployments(client().await?, ns);
        let d = match api.get_opt(deployment).await? {
            Some(d) => d,
            None => return Ok(None),
        };
        Ok(d.spec
            .and_then(|s| s.template.spec)
            .map(|ps| ps.containers)
            .and_then(|cs| cs.into_iter().find(|c| c.name == container))
            .and_then(|c| c.image))
    })?
}

/// A deployment's desired replica count, or `None` if the deployment isn't found.
pub fn get_replicas(ns: &str, deployment: &str) -> Result<Option<i32>> {
    block_on(async {
        let api = deployments(client().await?, ns);
        let d = match api.get_opt(deployment).await? {
            Some(d) => d,
            None => return Ok(None),
        };
        Ok(d.spec.and_then(|s| s.replicas))
    })?
}

/// `kubectl scale --replicas`: set a deployment's desired replica count.
pub fn set_replicas(ns: &str, deployment: &str, replicas: i32) -> Result<()> {
    block_on(async {
        let api = deployments(client().await?, ns);
        let patch = serde_json::json!({ "spec": { "replicas": replicas } });
        api.patch(deployment, &PatchParams::default(), &Patch::Merge(patch))
            .await
            .with_context(|| format!("scaling deploy/{deployment} to {replicas} replicas"))?;
        Ok(())
    })?
}

/// Resolve `container`'s index in `containers`. Exact name match wins; `kubectl set env` (what
/// `rig-config` uses) defaults to targeting every container in the pod, so on a single-container pod
/// we also accept any name; the caller doesn't have to know the exact container name to round-trip
/// the one container that exists.
fn container_index(
    containers: &[k8s_openapi::api::core::v1::Container],
    container: &str,
    deployment: &str,
) -> Result<usize> {
    if let Some(idx) = containers.iter().position(|c| c.name == container) {
        return Ok(idx);
    }
    if containers.len() == 1 {
        return Ok(0);
    }
    bail!(
        "container {container} not found in deploy/{deployment} (has {}: {})",
        containers.len(),
        containers
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Format `env` as `KEY=VALUE` lines, one per line, in order: the exact text `forge-kube get-env`
/// prints and `forge-kube set-env` reads back on stdin. A snapshot/restore hook pipes this straight
/// through as an opaque string; it must never be parsed into a list of pairs on the nu side, since nu
/// captures external-command stdout as a single string.
pub fn format_env_lines(env: &[(String, String)]) -> String {
    env.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse `KEY=VALUE` lines (as produced by [`format_env_lines`]) back into pairs, in order. Blank
/// lines are skipped so a trailing newline round-trips cleanly.
pub fn parse_env_lines(text: &str) -> Result<Vec<(String, String)>> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            l.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .with_context(|| format!("expected KEY=VALUE, got {l:?}"))
        })
        .collect()
}

/// Every literal env var on `container` in `deployment`, as `(name, value)` pairs in declaration
/// order: what `rig-config --set` writes, and what a snapshot/restore hook needs to round-trip so a
/// discarded candidate can't leave a `MOCK_*` knob tampered. `valueFrom`-sourced vars (secret/configmap
/// refs) are skipped; nothing on this path reads from those.
pub fn get_container_env(
    ns: &str,
    deployment: &str,
    container: &str,
) -> Result<Vec<(String, String)>> {
    block_on(async {
        let api = deployments(client().await?, ns);
        let d = api
            .get(deployment)
            .await
            .with_context(|| format!("getting deploy/{deployment}"))?;
        let containers = d
            .spec
            .and_then(|s| s.template.spec)
            .map(|ps| ps.containers)
            .unwrap_or_default();
        let idx = container_index(&containers, container, deployment)?;
        Ok(containers[idx]
            .env
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|e| e.value.map(|v| (e.name, v)))
            .collect())
    })?
}

/// Replace `container`'s literal env vars in `deployment` with exactly `env`: a JSON-patch `replace`
/// (not `kubectl set env`'s strategic merge), so a literal key the candidate added that the snapshot
/// never saw is removed too (restore must be a full replace). `valueFrom`-sourced vars are never in
/// `env` (see [`get_container_env`]) and are carried over from the live container untouched.
pub fn set_container_env(
    ns: &str,
    deployment: &str,
    container: &str,
    env: &[(String, String)],
) -> Result<()> {
    block_on(async {
        let api = deployments(client().await?, ns);
        let d = api
            .get(deployment)
            .await
            .with_context(|| format!("getting deploy/{deployment}"))?;
        let containers = d
            .spec
            .and_then(|s| s.template.spec)
            .map(|ps| ps.containers)
            .unwrap_or_default();
        let idx = container_index(&containers, container, deployment)?;
        let value_from_json: Vec<serde_json::Value> = containers[idx]
            .env
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter(|e| e.value_from.is_some())
            .map(|e| serde_json::to_value(e).context("serializing an existing env var"))
            .collect::<Result<_>>()?;
        let env_json: Vec<serde_json::Value> = value_from_json
            .into_iter()
            .chain(
                env.iter()
                    .map(|(k, v)| serde_json::json!({ "name": k, "value": v })),
            )
            .collect();
        let patch: json_patch::Patch = serde_json::from_value(serde_json::json!([
            { "op": "replace", "path": format!("/spec/template/spec/containers/{idx}/env"), "value": env_json }
        ]))?;
        api.patch(
            deployment,
            &PatchParams::default(),
            &Patch::Json::<()>(patch),
        )
        .await
        .with_context(|| format!("replacing env on deploy/{deployment} container {container}"))?;
        Ok(())
    })?
}

/// Read one `data` key of a ConfigMap, or `None` if the configmap/key is absent (e.g. a deployment
/// config a snapshot hook captures).
pub fn get_configmap_key(ns: &str, name: &str, key: &str) -> Result<Option<String>> {
    block_on(async {
        let api: Api<ConfigMap> = Api::namespaced(client().await?, ns);
        let cm = match api.get_opt(name).await? {
            Some(c) => c,
            None => return Ok(None),
        };
        Ok(cm.data.and_then(|d| d.get(key).cloned()))
    })?
}

/// Merge-patch one `data` key of a ConfigMap (leaving other keys untouched): the restore counterpart.
pub fn set_configmap_key(ns: &str, name: &str, key: &str, value: &str) -> Result<()> {
    block_on(async {
        let api: Api<ConfigMap> = Api::namespaced(client().await?, ns);
        let patch = serde_json::json!({ "data": { key: value } });
        api.patch(name, &PatchParams::default(), &Patch::Merge(patch))
            .await
            .with_context(|| format!("patching configmap {name} key {key}"))?;
        Ok(())
    })?
}

/// Delete a namespace, ignoring a not-found (idempotent teardown, like `kubectl delete ns --ignore-not-found`).
pub fn delete_namespace(name: &str) -> Result<()> {
    block_on(async {
        let api: Api<Namespace> = Api::all(client().await?);
        match api.delete(name, &DeleteParams::default()).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
            Err(e) => Err(e).with_context(|| format!("deleting namespace {name}")),
        }
    })?
}

/// Every pod matching `selector` (`k=v,k2=v2`): `ns` restricts to one namespace, `None` lists across
/// every namespace the client's identity can see (`crucible ps`'s default). A `list` RBAC denial comes
/// back as a clear, actionable error naming the missing permission instead of the raw 403.
pub fn list_pods(ns: Option<&str>, selector: &str) -> Result<Vec<Pod>> {
    block_on(async {
        let c = client().await?;
        let api: Api<Pod> = match ns {
            Some(ns) => Api::namespaced(c, ns),
            None => Api::all(c),
        };
        match api.list(&ListParams::default().labels(selector)).await {
            Ok(list) => Ok(list.items),
            Err(kube::Error::Api(e)) if e.code == 403 => match ns {
                Some(ns) => bail!(
                    "listing pods in namespace {ns} was denied: {} (needs a Role/RoleBinding granting \
                     `list` on pods in {ns})",
                    e.message
                ),
                None => bail!(
                    "listing pods cluster-wide was denied: {} (needs a ClusterRole/ClusterRoleBinding \
                     granting `list` on pods across all namespaces; pass --namespace to scope to one \
                     namespace you do have access to)",
                    e.message
                ),
            },
            Err(e) => Err(e).with_context(|| format!("listing pods by selector {selector}")),
        }
    })?
}

/// The name of the first pod matching `selector` (`k=v,k2=v2`) in `ns`, or `None` if none match.
pub fn pod_name_by_label(ns: &str, selector: &str) -> Result<Option<String>> {
    block_on(async {
        let api: Api<Pod> = Api::namespaced(client().await?, ns);
        let pods = api
            .list(&ListParams::default().labels(selector))
            .await
            .with_context(|| format!("listing pods by selector {selector}"))?;
        Ok(pods.items.into_iter().find_map(|p| p.metadata.name))
    })?
}

/// Logs from the crashlooping / not-ready replicas of `deployment`: the traceback behind a failed
/// rollout. Selector comes from the deployment's own `matchLabels`; the PREVIOUS (terminated)
/// instance is preferred (a CrashLoopBackOff pod's fatal traceback lives there), falling back to the
/// current one. Empty when every replica is ready; per-pod log errors are skipped rather than fatal.
pub fn crash_logs(ns: &str, deployment: &str, container: &str, tail: i64) -> Result<String> {
    block_on(async {
        let client = client().await?;
        let deploys: Api<Deployment> = Api::namespaced(client.clone(), ns);
        let d = deploys
            .get(deployment)
            .await
            .with_context(|| format!("getting deploy/{deployment} for crash logs"))?;
        let selector = d
            .spec
            .and_then(|s| s.selector.match_labels)
            .map(|m| {
                m.iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        let api: Api<Pod> = Api::namespaced(client, ns);
        let pods = api
            .list(&ListParams::default().labels(&selector))
            .await
            .with_context(|| format!("listing pods of deploy/{deployment}"))?;

        let mut out = String::new();
        for p in pods.items {
            let Some(name) = p.metadata.name.clone() else {
                continue;
            };
            let unready = p
                .status
                .as_ref()
                .and_then(|s| s.container_statuses.as_ref())
                .map(|cs| cs.iter().any(|c| c.name == container && !c.ready))
                .unwrap_or(false);
            if !unready {
                continue;
            }
            // Prefer the crashed (previous) instance; fall back to the current one.
            let body = match fetch_pod_logs(&api, &name, container, tail, true).await {
                Ok(l) if !l.trim().is_empty() => l,
                _ => fetch_pod_logs(&api, &name, container, tail, false)
                    .await
                    .unwrap_or_default(),
            };
            if !body.trim().is_empty() {
                out.push_str(&format!(
                    "----- {name} ({container}) -----\n{}\n",
                    body.trim_end()
                ));
            }
        }
        Ok(out)
    })?
}

/// Tail `container`'s logs in `pod`; `previous` reads the last terminated instance (the crash traceback).
async fn fetch_pod_logs(
    api: &Api<Pod>,
    pod: &str,
    container: &str,
    tail: i64,
    previous: bool,
) -> Result<String> {
    let lp = LogParams {
        container: Some(container.to_string()),
        tail_lines: Some(tail),
        previous,
        ..Default::default()
    };
    api.logs(pod, &lp)
        .await
        .with_context(|| format!("fetching logs for pod {pod}"))
}

/// `kubectl exec`: run `command` in `container` of `pod` and return its captured stdout. Domain
/// invariants that read a pod-local endpoint (`curl localhost:…/metrics`) go through here.
pub fn exec(ns: &str, pod: &str, container: &str, command: &[&str]) -> Result<String> {
    block_on(async {
        let api: Api<Pod> = Api::namespaced(client().await?, ns);
        let ap = AttachParams::default()
            .container(container)
            .stdout(true)
            .stderr(false);
        let mut attached = api
            .exec(pod, command.iter().copied(), &ap)
            .await
            .with_context(|| format!("exec in {pod}/{container}"))?;
        let mut out = String::new();
        if let Some(mut stdout) = attached.stdout() {
            use tokio::io::AsyncReadExt;
            stdout
                .read_to_string(&mut out)
                .await
                .context("reading exec stdout")?;
        }
        attached.join().await.context("awaiting exec completion")?;
        Ok(out)
    })?
}

/// Free GPUs across the cluster = Σ node allocatable `nvidia.com/gpu` − Σ requested by running pods.
/// The `gpu-check` admission signal, typed off real `Node`/`Pod` objects instead of jsonpath-walking.
pub fn free_gpus() -> Result<u32> {
    block_on(async {
        let c = client().await?;
        let nodes: Api<Node> = Api::all(c.clone());
        let pods: Api<Pod> = Api::all(c);
        let nodes = nodes
            .list(&ListParams::default())
            .await
            .context("listing nodes")?;
        let running = pods
            .list(&ListParams::default().fields("status.phase=Running"))
            .await
            .context("listing running pods")?;
        Ok(sum_node_gpus(&nodes.items).saturating_sub(sum_pod_gpu_requests(&running.items)))
    })?
}

const GPU: &str = "nvidia.com/gpu";

/// Σ allocatable `nvidia.com/gpu` over `nodes`; pure, so it's unit-testable off constructed objects.
fn sum_node_gpus(nodes: &[Node]) -> u32 {
    nodes
        .iter()
        .map(|n| {
            n.status
                .as_ref()
                .and_then(|s| s.allocatable.as_ref())
                .and_then(|a| a.get(GPU))
                .map(gpu_qty)
                .unwrap_or(0)
        })
        .sum()
}

/// Σ requested `nvidia.com/gpu` across every container of `pods`.
fn sum_pod_gpu_requests(pods: &[Pod]) -> u32 {
    pods.iter()
        .flat_map(|p| {
            p.spec
                .as_ref()
                .map(|s| s.containers.iter())
                .into_iter()
                .flatten()
        })
        .map(|c| {
            c.resources
                .as_ref()
                .and_then(|r| r.requests.as_ref())
                .and_then(|r| r.get(GPU))
                .map(gpu_qty)
                .unwrap_or(0)
        })
        .sum()
}

/// Server-side apply a multi-document YAML string (the deploy-render output, generated capture jobs).
/// Resolves each doc's GroupVersionKind by API discovery, so it handles any kind without the engine
/// knowing the type: the typed equivalent of `kubectl apply -f -`.
pub fn apply_yaml(yaml: &str) -> Result<()> {
    block_on(async {
        let c = client().await?;
        let discovery = Discovery::new(c.clone())
            .run()
            .await
            .context("running API discovery for apply")?;
        for doc in yaml.split("\n---").map(str::trim).filter(|d| !d.is_empty()) {
            let obj: DynamicObject =
                serde_norway::from_str(doc).context("parsing a YAML document for apply")?;
            let gvk = gvk_of(&obj)?;
            let name = obj
                .metadata
                .name
                .clone()
                .context("apply: object has no metadata.name")?;
            let (ar, caps) = discovery
                .resolve_gvk(&gvk)
                .with_context(|| format!("resolving {}.{} via discovery", gvk.kind, gvk.group))?;
            let api: Api<DynamicObject> = match caps.scope {
                Scope::Namespaced => {
                    let ns = obj.metadata.namespace.as_deref().unwrap_or("default");
                    Api::namespaced_with(c.clone(), ns, &ar)
                }
                Scope::Cluster => Api::all_with(c.clone(), &ar),
            };
            api.patch(
                &name,
                &PatchParams::apply("forge").force(),
                &Patch::Apply(&obj),
            )
            .await
            .with_context(|| format!("applying {}/{name}", gvk.kind))?;
        }
        Ok(())
    })?
}

fn deployments(client: Client, ns: &str) -> Api<Deployment> {
    Api::namespaced(client, ns)
}

/// The terminal state of a build Job. `Succeeded`/`Failed` are derived from the Job's
/// `Complete`/`Failed` conditions (the signal `kubectl wait --for=condition=complete` reads);
/// `TimedOut` is our own wall-clock deadline firing before either condition; the caller reaps the
/// wedged Job so it can't hold a deployment slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobResult {
    Succeeded,
    Failed,
    TimedOut,
}

/// Create a detached build Job and return its UID (so a per-build push secret can be owner-ref'd
/// to it and cascade on TTL-reap). The controller never streams a build context; the Job clones
/// its own source.
pub(crate) fn create_job(ns: &str, job: &Job) -> Result<String> {
    block_on(async {
        let api: Api<Job> = Api::namespaced(client().await?, ns);
        let created = api
            .create(&PostParams::default(), job)
            .await
            .with_context(|| format!("creating build Job in {ns}"))?;
        created.metadata.uid.context("the created Job has no uid")
    })?
}

/// Create a per-dispatch build secret (the registry push `config.json`, or the optional git clone
/// token) the build Job mounts, owner-ref'd to the Job so a TTL-reap of the Job cascades the secret
/// away (buildit's pattern, no dangling creds). `key` is the single data key (and mounted filename);
/// the secret name is unique per dispatch, so this only ever creates.
pub(crate) fn create_build_secret(
    ns: &str,
    name: &str,
    job_name: &str,
    job_uid: &str,
    key: &str,
    value: &str,
) -> Result<()> {
    block_on(async {
        let api: Api<Secret> = Api::namespaced(client().await?, ns);
        let secret = Secret {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some(ns.to_string()),
                owner_references: Some(vec![OwnerReference {
                    api_version: "batch/v1".to_string(),
                    kind: "Job".to_string(),
                    name: job_name.to_string(),
                    uid: job_uid.to_string(),
                    controller: Some(true),
                    // No blockOwnerDeletion: garbage collection already cascades the secret from the
                    // ownerReference, and setting it would demand `update` on batch/jobs/finalizers,
                    // a permission the scoped M1 SA lacks, which would reject AFTER the Job is created
                    // and orphan it.
                    block_owner_deletion: None,
                }]),
                ..Default::default()
            },
            type_: Some("Opaque".to_string()),
            string_data: Some(std::collections::BTreeMap::from([(
                key.to_string(),
                value.to_string(),
            )])),
            ..Default::default()
        };
        api.create(&PostParams::default(), &secret)
            .await
            .with_context(|| format!("creating build secret {name} in {ns}"))?;
        Ok(())
    })?
}

/// Poll a build Job to a terminal condition or `timeout`. On timeout returns [`JobResult::TimedOut`]
/// (not an error) so the caller can reap the wedged Job before failing. A single `block_on` owns the
/// whole wait, like [`rollout_status`].
pub(crate) fn wait_for_job(ns: &str, name: &str, timeout: Duration) -> Result<JobResult> {
    block_on(async {
        let api: Api<Job> = Api::namespaced(client().await?, ns);
        let deadline = Instant::now() + timeout;
        loop {
            let job = api
                .get(name)
                .await
                .with_context(|| format!("getting build Job {name} for status"))?;
            if let Some(conditions) = job.status.as_ref().and_then(|s| s.conditions.as_ref()) {
                for c in conditions {
                    if c.status != "True" {
                        continue;
                    }
                    match c.type_.as_str() {
                        "Complete" => return Ok(JobResult::Succeeded),
                        "Failed" => return Ok(JobResult::Failed),
                        _ => {}
                    }
                }
            }
            if Instant::now() >= deadline {
                return Ok(JobResult::TimedOut);
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    })?
}

/// Delete a build Job and its pods (propagation `Background`), ignoring a not-found. Used to reap a
/// wedged Job when the wait times out so it can't hold cluster resources past its deadline.
pub(crate) fn delete_job(ns: &str, name: &str) -> Result<()> {
    block_on(async {
        let api: Api<Job> = Api::namespaced(client().await?, ns);
        let dp = DeleteParams {
            propagation_policy: Some(kube::api::PropagationPolicy::Background),
            ..Default::default()
        };
        match api.delete(name, &dp).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
            Err(e) => Err(e).with_context(|| format!("deleting build Job {name} in {ns}")),
        }
    })?
}

/// Byte budget for a full (untailed) log fetch: big enough for any real measure log, small enough
/// that a runaway printer can't balloon the broker. It caps the log's HEAD (not a sliding tail),
/// so a capped log stops growing but never slides, and reads by offset stay stable.
const FULL_LOG_BYTE_BUDGET: i64 = 64 * 1024 * 1024;

/// Combined logs of `container` across the Job's pod(s) (selected by the standard `job-name` label).
/// `tail = Some(n)` keeps the last n lines (a failure-evidence pointer); `None` fetches the whole
/// log (byte-budgeted) so the caller sees a GROWING PREFIX, required by live tailing, where a
/// sliding window would tear a concurrent reader's offset. Best-effort per pod: a per-pod log error
/// is skipped rather than fatal.
pub(crate) fn job_logs(
    ns: &str,
    job_name: &str,
    container: &str,
    tail: Option<i64>,
) -> Result<String> {
    block_on(async {
        let api: Api<Pod> = Api::namespaced(client().await?, ns);
        let pods = api
            .list(&ListParams::default().labels(&format!("job-name={job_name}")))
            .await
            .with_context(|| format!("listing pods of build Job {job_name}"))?;
        let mut out = String::new();
        for p in pods.items {
            let Some(name) = p.metadata.name.clone() else {
                continue;
            };
            let lp = LogParams {
                container: Some(container.to_string()),
                tail_lines: tail,
                limit_bytes: tail.is_none().then_some(FULL_LOG_BYTE_BUDGET),
                ..Default::default()
            };
            if let Ok(body) = api.logs(&name, &lp).await
                && !body.trim().is_empty()
            {
                out.push_str(&format!(
                    "----- {name} ({container}) -----\n{}\n",
                    body.trim_end()
                ));
            }
        }
        Ok(out)
    })?
}

/// The Kueue admission view of a submitted Job's Workload.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KueueStatus {
    pub quota_reserved: bool,
    pub admitted: bool,
    /// The message of a False QuotaReserved/Admitted condition (why the job still waits).
    pub pending_reason: Option<String>,
}

/// Every Kueue Workload status in `ns`, keyed by the OWNING Job's name. ONE list per call, so a
/// poller with K in-flight jobs costs one Workload list instead of K. Read-only; reports queue
/// state back to the jobs' own submitter.
pub fn workload_statuses(ns: &str) -> Result<std::collections::HashMap<String, KueueStatus>> {
    block_on(async {
        let gvk = GroupVersionKind {
            group: "kueue.x-k8s.io".to_string(),
            version: "v1beta1".to_string(),
            kind: "Workload".to_string(),
        };
        let ar = kube::core::ApiResource::from_gvk(&gvk);
        let api: Api<DynamicObject> = Api::namespaced_with(client().await?, ns, &ar);
        let workloads = api
            .list(&ListParams::default())
            .await
            .with_context(|| format!("listing Kueue Workloads in {ns}"))?;
        let mut by_job = std::collections::HashMap::new();
        for w in workloads.items {
            let Some(owner) = w
                .metadata
                .owner_references
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find(|o| o.kind == "Job")
            else {
                continue;
            };
            let conds = w
                .data
                .pointer("/status/conditions")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            by_job.insert(owner.name.clone(), parse_workload_conditions(&conds));
        }
        Ok(by_job)
    })?
}

/// Fold a Workload's `status.conditions` into [`KueueStatus`] (pure, unit-testable on synthetic
/// condition sets). A False QuotaReserved or Admitted condition contributes its message as the
/// pending reason (QuotaReserved's wins: it is the earlier, more specific "why it waits").
fn parse_workload_conditions(conditions: &serde_json::Value) -> KueueStatus {
    let mut st = KueueStatus::default();
    let Some(conds) = conditions.as_array() else {
        return st;
    };
    for c in conds {
        let type_ = c
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let is_true = c.get("status").and_then(serde_json::Value::as_str) == Some("True");
        let message = c
            .get("message")
            .and_then(serde_json::Value::as_str)
            .filter(|m| !m.is_empty())
            .map(str::to_string);
        match type_ {
            "QuotaReserved" => {
                st.quota_reserved = is_true;
                if !is_true {
                    st.pending_reason = message.or(st.pending_reason.take());
                }
            }
            "Admitted" => {
                st.admitted = is_true;
                if !is_true && st.pending_reason.is_none() {
                    st.pending_reason = message;
                }
            }
            _ => {}
        }
    }
    st
}

/// A submitted Job's pod-level progress: the first matching pod's phase + start time (a measure Job
/// runs exactly one pod: `backoffLimit: 0`, no restarts).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PodStatus {
    pub phase: Option<String>,
    pub started_at: Option<String>,
}

/// The pod phase/start of a Job's pod (by the standard `job-name` label); `None` fields until one
/// is scheduled. Read-only; no pod names leave this boundary.
pub fn job_pod_status(ns: &str, job_name: &str) -> Result<PodStatus> {
    block_on(async {
        let api: Api<Pod> = Api::namespaced(client().await?, ns);
        let pods = api
            .list(&ListParams::default().labels(&format!("job-name={job_name}")))
            .await
            .with_context(|| format!("listing pods of Job {job_name}"))?;
        let st = pods
            .items
            .first()
            .and_then(|p| p.status.as_ref())
            .map(|s| PodStatus {
                phase: s.phase.clone(),
                // jiff's Display for a Timestamp IS RFC 3339.
                started_at: s.start_time.as_ref().map(|t| t.0.to_string()),
            })
            .unwrap_or_default();
        Ok(st)
    })?
}

/// A k8s quantity holding a GPU count is always a small integer string (`"1"`, `"8"`); parse it, 0 on
/// anything unexpected (fractional GPUs aren't a thing).
fn gpu_qty(q: &k8s_openapi::apimachinery::pkg::api::resource::Quantity) -> u32 {
    q.0.parse().unwrap_or(0)
}

/// The GroupVersionKind from a dynamic object's `apiVersion`/`kind` (`apps/v1` → group `apps`, version
/// `v1`; a core `v1` → empty group).
fn gvk_of(obj: &DynamicObject) -> Result<GroupVersionKind> {
    let TypeMeta { api_version, kind } = obj
        .types
        .clone()
        .context("apply: object has no apiVersion/kind")?;
    let (group, version) = match api_version.split_once('/') {
        Some((g, v)) => (g.to_string(), v.to_string()),
        None => (String::new(), api_version),
    };
    Ok(GroupVersionKind {
        group,
        version,
        kind,
    })
}

/// A coarse RFC3339-ish timestamp for the restart annotation. The value only needs to *change* to trigger
/// a roll; uniqueness across seconds is plenty, so seconds-since-epoch in a stable shape is enough.
fn rfc3339_nowish() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("forge-{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workload_conditions_fold_into_kueue_status() {
        // Freshly queued: quota not reserved yet, the message says why.
        let queued = serde_json::json!([
            {"type": "QuotaReserved", "status": "False", "message": "couldn't assign flavors: insufficient quota for nvidia.com/gpu"}
        ]);
        let st = parse_workload_conditions(&queued);
        assert!(!st.quota_reserved && !st.admitted);
        assert_eq!(
            st.pending_reason.as_deref(),
            Some("couldn't assign flavors: insufficient quota for nvidia.com/gpu")
        );

        // Admitted: both true, no pending reason.
        let admitted = serde_json::json!([
            {"type": "QuotaReserved", "status": "True", "message": "Quota reserved"},
            {"type": "Admitted", "status": "True", "message": "The workload is admitted"}
        ]);
        let st = parse_workload_conditions(&admitted);
        assert!(st.quota_reserved && st.admitted);
        assert_eq!(st.pending_reason, None);

        // QuotaReserved's False message beats Admitted's (the specific why-it-waits).
        let both_false = serde_json::json!([
            {"type": "Admitted", "status": "False", "message": "not admitted"},
            {"type": "QuotaReserved", "status": "False", "message": "queue is full"}
        ]);
        assert_eq!(
            parse_workload_conditions(&both_false)
                .pending_reason
                .as_deref(),
            Some("queue is full")
        );

        // No/odd status shapes degrade to all-false, never panic.
        assert_eq!(
            parse_workload_conditions(&serde_json::Value::Null),
            KueueStatus::default()
        );
        assert_eq!(
            parse_workload_conditions(&serde_json::json!({"not": "an array"})),
            KueueStatus::default()
        );
        // Unknown condition types are ignored.
        let extra = serde_json::json!([
            {"type": "Finished", "status": "True"},
            {"type": "QuotaReserved", "status": "True"}
        ]);
        assert!(parse_workload_conditions(&extra).quota_reserved);
    }

    #[test]
    fn gvk_parses_grouped_and_core() {
        let mk = |av: &str, kind: &str| {
            let mut o = DynamicObject::new("x", &kube::core::ApiResource::erase::<Pod>(&()));
            o.types = Some(TypeMeta {
                api_version: av.to_string(),
                kind: kind.to_string(),
            });
            gvk_of(&o).unwrap()
        };
        let apps = mk("apps/v1", "Deployment");
        assert_eq!((apps.group.as_str(), apps.version.as_str()), ("apps", "v1"));
        let core = mk("v1", "Pod");
        assert_eq!((core.group.as_str(), core.version.as_str()), ("", "v1"));
        let rbac = mk("rbac.authorization.k8s.io/v1", "RoleBinding");
        assert_eq!(rbac.group, "rbac.authorization.k8s.io");
    }

    #[test]
    fn gpu_qty_parses_counts() {
        use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
        assert_eq!(gpu_qty(&Quantity("8".into())), 8);
        assert_eq!(gpu_qty(&Quantity("".into())), 0);
        assert_eq!(gpu_qty(&Quantity("garbage".into())), 0);
    }

    #[test]
    fn sums_node_allocatable_and_pod_requests() {
        // Two 8-GPU nodes + one with none allocatable -> 16.
        let nodes: Vec<Node> = serde_json::from_value(serde_json::json!([
            { "status": { "allocatable": { "nvidia.com/gpu": "8" } } },
            { "status": { "allocatable": { "nvidia.com/gpu": "8" } } },
            { "status": { "allocatable": {} } }
        ]))
        .unwrap();
        assert_eq!(sum_node_gpus(&nodes), 16);

        // 2 + 1 across one pod's two containers; a second pod requests none -> 3.
        let pods: Vec<Pod> = serde_json::from_value(serde_json::json!([
            { "spec": { "containers": [
                { "name": "a", "resources": { "requests": { "nvidia.com/gpu": "2" } } },
                { "name": "b", "resources": { "requests": { "nvidia.com/gpu": "1" } } }
            ] } },
            { "spec": { "containers": [ { "name": "c", "resources": {} } ] } }
        ]))
        .unwrap();
        assert_eq!(sum_pod_gpu_requests(&pods), 3);
    }

    fn containers(names: &[&str]) -> Vec<k8s_openapi::api::core::v1::Container> {
        serde_json::from_value(serde_json::json!(
            names
                .iter()
                .map(|n| serde_json::json!({ "name": n }))
                .collect::<Vec<_>>()
        ))
        .unwrap()
    }

    #[test]
    fn container_index_prefers_an_exact_name_match() {
        let cs = containers(&["mock-prefix-cache", "sidecar"]);
        assert_eq!(container_index(&cs, "sidecar", "d").unwrap(), 1);
    }

    #[test]
    fn container_index_falls_back_on_a_single_container_pod() {
        // `kubectl set env` (what rig-config uses) defaults to every container in the pod, so a
        // caller naming the wrong single container must still resolve instead of erroring.
        let cs = containers(&["whatever-its-actually-called"]);
        assert_eq!(container_index(&cs, "mock-prefix-cache", "d").unwrap(), 0);
    }

    #[test]
    fn container_index_errors_on_an_ambiguous_multi_container_miss() {
        let cs = containers(&["a", "b"]);
        let err = container_index(&cs, "c", "d").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn mock_env_round_trips_through_the_snapshot_token_json_shape() {
        // rig-snapshot.nu captures `forge-kube get-env`'s stdout as a plain STRING (nu captures
        // external-command output as a string, not a list) and stashes it in the token verbatim;
        // rig-restore.nu pipes that string straight back to `forge-kube set-env`'s stdin. So the
        // token's `mock_env` field must be a `KEY=VALUE`-lines string rather than a list of pairs.
        let env: Vec<(String, String)> = vec![
            ("MOCK_ITL_MS".to_string(), "5".to_string()),
            ("MODEL".to_string(), "Qwen/Qwen3-0.6B".to_string()),
        ];
        let env_lines = format_env_lines(&env);
        let token = serde_json::json!({
            "image": "img:tag",
            "plugins": "key: value\n",
            "mock_replicas": 3,
            "mock_env": env_lines,
        });
        let round_tripped: serde_json::Value = serde_json::from_str(&token.to_string()).unwrap();
        let env_back = parse_env_lines(round_tripped["mock_env"].as_str().unwrap()).unwrap();
        assert_eq!(
            env_back, env,
            "env pairs and order must survive the get-env -> JSON token -> set-env round-trip"
        );
        assert_eq!(round_tripped["mock_replicas"], 3);
    }
}
