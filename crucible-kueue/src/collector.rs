//! The collector container the server adds to a Job whose plan names a results directory, and the
//! process that runs in it: it waits for the Job's own containers to exit, then writes the
//! directory's files to its log in the [`crate::results`] format.

use crate::config::Config;
use crate::manifest::{self, ManifestError, Normalized, WorkloadKind};
use serde_json::{Map, Value, json};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use tokio::signal::unix::{SignalKind, signal};

/// The name of the volume and the container the server adds.
pub const NAME: &str = "crucible-results";

/// Where the collector container mounts the directory.
const MOUNT: &str = "/crucible-results";

pub const DEFAULT_MAX_BYTES: u64 = 16 * 1024 * 1024;

const POD_SPEC: &str = "/spec/template/spec";

/// The collector this server adds to pods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collector {
    /// An image holding this server's binary.
    pub image: String,
    /// The most bytes of files one pod's archive holds.
    pub max_bytes: u64,
}

/// A directory in a workload's containers whose files are collected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultsDir(String);

impl ResultsDir {
    pub fn parse(raw: &str) -> Result<Self, ManifestError> {
        let refused = || ManifestError::ResultsDir {
            got: raw.to_string(),
        };
        let path = Path::new(raw);
        let mut names = Vec::new();
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => names.push(name.to_str().ok_or_else(refused)?),
                _ => return Err(refused()),
            }
        }
        if !path.is_absolute() || names.is_empty() {
            return Err(refused());
        }
        Ok(Self(format!("/{}", names.join("/"))))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn taken(path: String, what: &'static str) -> ManifestError {
    ManifestError::Forbidden { path, what }
}

fn named(list: &[Value], path: &str) -> Result<(), ManifestError> {
    match list
        .iter()
        .position(|v| v.get("name").and_then(Value::as_str) == Some(NAME))
    {
        Some(i) => Err(taken(
            format!("{path}/{i}/name"),
            "the name crucible-results, which the results collector takes,",
        )),
        None => Ok(()),
    }
}

fn list<'a>(
    spec: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Vec<Value>, ManifestError> {
    spec.entry(key)
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| ManifestError::Shape {
            path: format!("{POD_SPEC}/{key}"),
            expected: "an array",
        })
}

fn container(collector: &Collector, run_as: Map<String, Value>) -> Value {
    let mut security = json!({
        "allowPrivilegeEscalation": false,
        "capabilities": {"drop": ["ALL"]},
        "readOnlyRootFilesystem": true,
        "runAsNonRoot": true,
        "seccompProfile": {"type": "RuntimeDefault"},
    });
    if let Some(security) = security.as_object_mut() {
        security.extend(run_as);
    }
    json!({
        "name": NAME,
        "image": collector.image,
        "restartPolicy": "Always",
        "command": [
            "crucible-kueue",
            "collect",
            "--dir",
            MOUNT,
            "--max-bytes",
            collector.max_bytes.to_string(),
        ],
        "volumeMounts": [{"name": NAME, "mountPath": MOUNT, "readOnly": true}],
        "resources": {
            "requests": {"cpu": "10m", "memory": "32Mi"},
            "limits": {"cpu": "500m", "memory": "64Mi"},
        },
        "securityContext": security,
    })
}

impl Normalized {
    /// Collect the files the Job's containers leave in `dir`: a volume mounted there in each of
    /// them, and a container that writes its files to its own log once they have exited. Both
    /// are part of the object, so they change its hash.
    pub fn collecting(
        mut self,
        dir: Option<&ResultsDir>,
        cfg: &Config,
    ) -> Result<Self, ManifestError> {
        let Some(dir) = dir else {
            return Ok(self);
        };
        if self.kind != WorkloadKind::Job {
            return Err(ManifestError::ResultsKind {
                kind: self.kind.kind(),
            });
        }
        let Some(collector) = &cfg.results else {
            return Err(ManifestError::NoCollector);
        };
        let spec = self
            .object
            .pointer_mut(POD_SPEC)
            .and_then(Value::as_object_mut)
            .ok_or(ManifestError::NoTemplate {
                kind: WorkloadKind::Job.kind(),
                path: POD_SPEC.to_string(),
            })?;
        named(list(spec, "volumes")?, &format!("{POD_SPEC}/volumes"))?;
        named(
            list(spec, "initContainers")?,
            &format!("{POD_SPEC}/initContainers"),
        )?;
        let containers = list(spec, "containers")?;
        named(containers, &format!("{POD_SPEC}/containers"))?;
        let mut run_as = Map::new();
        for (i, c) in containers.iter_mut().enumerate() {
            let path = format!("{POD_SPEC}/containers/{i}");
            let c = c.as_object_mut().ok_or_else(|| ManifestError::Shape {
                path: path.clone(),
                expected: "an object",
            })?;
            if i == 0 {
                for key in ["runAsUser", "runAsGroup"] {
                    if let Some(id) = c.get("securityContext").and_then(|s| s.get(key)) {
                        run_as.insert(key.to_string(), id.clone());
                    }
                }
            }
            let mounts = c
                .entry("volumeMounts")
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
                .ok_or_else(|| ManifestError::Shape {
                    path: format!("{path}/volumeMounts"),
                    expected: "an array",
                })?;
            let at = |m: &Value| {
                m.get("mountPath")
                    .and_then(Value::as_str)
                    .is_some_and(|p| p.trim_end_matches('/') == dir.as_str())
            };
            if let Some(j) = mounts.iter().position(at) {
                return Err(taken(
                    format!("{path}/volumeMounts/{j}/mountPath"),
                    "a mount at the results directory, where the server mounts its own volume,",
                ));
            }
            mounts.push(json!({"name": NAME, "mountPath": dir.as_str()}));
        }
        list(spec, "volumes")?.push(json!({"name": NAME, "emptyDir": {}}));
        list(spec, "initContainers")?.push(container(collector, run_as));
        self.server_changes.push(format!(
            "{POD_SPEC}: added the {NAME} volume at {} and the {NAME} container, which writes the files there to its log when the Job's containers exit",
            dir.as_str()
        ));
        self.server_changes
            .extend(manifest::add_pull_secrets(spec, POD_SPEC, cfg)?);
        self.pod_sets = manifest::pod_sets_of(self.kind, &self.object)?;
        self.rehash();
        Ok(self)
    }
}

/// Whether a cluster of this version runs an init container that restarts always as a sidecar,
/// which Kubernetes does from 1.29.
pub fn runs_sidecars(major: &str, minor: &str) -> bool {
    let number = |s: &str| {
        s.trim_end_matches(|c: char| !c.is_ascii_digit())
            .parse::<u32>()
            .ok()
    };
    match (number(major), number(minor)) {
        (Some(major), Some(minor)) => major > 1 || (major == 1 && minor >= 29),
        _ => false,
    }
}

/// Whether `pod` carries the collector.
pub fn collects(pod: &k8s_openapi::api::core::v1::Pod) -> bool {
    pod.spec
        .as_ref()
        .and_then(|s| s.init_containers.as_ref())
        .is_some_and(|l| l.iter().any(|c| c.name == NAME))
}

/// Waits for SIGTERM, which the kubelet sends once the pod's own containers have exited, then
/// writes the files under `dir` to stdout.
pub async fn collect(dir: PathBuf, max_bytes: u64) -> std::io::Result<()> {
    let mut term = signal(SignalKind::terminate())?;
    term.recv().await;
    tokio::task::spawn_blocking(move || {
        let mut out = std::io::BufWriter::new(std::io::stdout().lock());
        let written = crate::results::write(&dir, max_bytes, &mut out);
        out.flush()?;
        written
    })
    .await?
}

#[cfg(test)]
mod tests {
    use crate::caller::{Caller, QueueName};
    use crate::collector::*;
    use crate::config::PullSecret;
    use crate::manifest::normalize;

    fn collecting_cfg() -> Config {
        Config {
            results: Some(Collector {
                image: "quay.io/crucible/crucible-kueue:1".into(),
                max_bytes: 4096,
            }),
            ..Config::default()
        }
    }

    fn job(pod: Value) -> Value {
        json!({
            "apiVersion": "batch/v1",
            "kind": "Job",
            "metadata": {"name": "bench"},
            "spec": {"template": {"spec": pod}},
        })
    }

    fn bench() -> Value {
        job(json!({
            "restartPolicy": "Never",
            "containers": [{
                "name": "main",
                "image": "ubi9",
                "resources": {"requests": {"cpu": "2", "memory": "1Gi"}},
            }],
        }))
    }

    fn planned(manifest: Value, dir: &str, cfg: &Config) -> Result<Normalized, ManifestError> {
        let queue = QueueName::parse("gpu").unwrap();
        normalize(manifest, &Caller::for_test(), &queue, cfg)?
            .collecting(Some(&ResultsDir::parse(dir)?), cfg)
    }

    #[test]
    fn a_job_gets_the_volume_in_its_containers_and_the_collector_as_a_sidecar() {
        let n = planned(bench(), "/work/out/", &collecting_cfg()).unwrap();
        let spec = n.object.pointer(POD_SPEC).unwrap();
        assert_eq!(
            spec["volumes"],
            json!([{"name": "crucible-results", "emptyDir": {}}])
        );
        assert_eq!(
            spec["containers"][0]["volumeMounts"],
            json!([{"name": "crucible-results", "mountPath": "/work/out"}])
        );
        assert_eq!(
            spec["initContainers"],
            json!([{
                "name": "crucible-results",
                "image": "quay.io/crucible/crucible-kueue:1",
                "restartPolicy": "Always",
                "command": ["crucible-kueue", "collect", "--dir", "/crucible-results", "--max-bytes", "4096"],
                "volumeMounts": [{"name": "crucible-results", "mountPath": "/crucible-results", "readOnly": true}],
                "resources": {
                    "requests": {"cpu": "10m", "memory": "32Mi"},
                    "limits": {"cpu": "500m", "memory": "64Mi"},
                },
                "securityContext": {
                    "allowPrivilegeEscalation": false,
                    "capabilities": {"drop": ["ALL"]},
                    "readOnlyRootFilesystem": true,
                    "runAsNonRoot": true,
                    "seccompProfile": {"type": "RuntimeDefault"},
                },
            }])
        );
        assert_eq!(
            n.server_changes,
            [
                "/spec/template/spec: added the crucible-results volume at /work/out and the crucible-results container, which writes the files there to its log when the Job's containers exit"
            ]
        );
    }

    #[test]
    fn the_collector_is_part_of_the_plan_hash_and_the_requests() {
        let cfg = collecting_cfg();
        let queue = QueueName::parse("gpu").unwrap();
        let plain = normalize(bench(), &Caller::for_test(), &queue, &cfg).unwrap();
        assert_eq!(
            plain.clone().collecting(None, &cfg).unwrap().plan_hash,
            plain.plan_hash
        );
        let a = planned(bench(), "/out", &cfg).unwrap();
        let b = planned(bench(), "/other", &cfg).unwrap();
        assert_ne!(a.plan_hash, plain.plan_hash);
        assert_ne!(a.plan_hash, b.plan_hash);
        assert_eq!(
            a.plan_hash,
            planned(bench(), "/out", &cfg).unwrap().plan_hash
        );
        let exposed = planned(bench(), "/out", &cfg)
            .unwrap()
            .exposing(&[8000])
            .unwrap();
        assert_ne!(exposed.plan_hash, a.plan_hash);
        assert_eq!(
            exposed.plan_hash,
            normalize(bench(), &Caller::for_test(), &queue, &cfg)
                .unwrap()
                .exposing(&[8000])
                .unwrap()
                .collecting(Some(&ResultsDir::parse("/out").unwrap()), &cfg)
                .unwrap()
                .plan_hash
        );

        let q = |s: &str| kube_quantity::ParsedQuantity::try_from(s).unwrap();
        let requests = a.total_requests();
        assert_eq!(requests["cpu"], q("2010m"));
        assert_eq!(requests["memory"], q("1056Mi"));
    }

    #[test]
    fn the_collector_runs_as_the_first_containers_user() {
        let mut m = bench();
        m["spec"]["template"]["spec"]["containers"][0]["securityContext"] =
            json!({"runAsUser": 1000, "runAsGroup": 3000, "runAsNonRoot": true});
        let n = planned(m, "/out", &collecting_cfg()).unwrap();
        assert_eq!(
            n.object
                .pointer(&format!("{POD_SPEC}/initContainers/0/securityContext")),
            Some(&json!({
                "allowPrivilegeEscalation": false,
                "capabilities": {"drop": ["ALL"]},
                "readOnlyRootFilesystem": true,
                "runAsNonRoot": true,
                "seccompProfile": {"type": "RuntimeDefault"},
                "runAsUser": 1000,
                "runAsGroup": 3000,
            }))
        );
    }

    #[test]
    fn every_container_gets_the_mount_and_the_collector_starts_after_the_jobs_own_init() {
        let m = job(json!({
            "restartPolicy": "Never",
            "initContainers": [{"name": "setup", "image": "ubi9"}],
            "volumes": [{"name": "scratch", "emptyDir": {}}],
            "containers": [
                {"name": "a", "image": "ubi9", "volumeMounts": [{"name": "scratch", "mountPath": "/scratch"}]},
                {"name": "b", "image": "ubi9"},
            ],
        }));
        let n = planned(m, "/out", &collecting_cfg()).unwrap();
        let spec = n.object.pointer(POD_SPEC).unwrap();
        let names = |list: &str| -> Vec<&str> {
            spec[list]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["name"].as_str().unwrap())
                .collect()
        };
        assert_eq!(names("initContainers"), ["setup", "crucible-results"]);
        assert_eq!(names("volumes"), ["scratch", "crucible-results"]);
        assert_eq!(
            spec["containers"][0]["volumeMounts"],
            json!([
                {"name": "scratch", "mountPath": "/scratch"},
                {"name": "crucible-results", "mountPath": "/out"},
            ])
        );
        assert_eq!(
            spec["containers"][1]["volumeMounts"],
            json!([{"name": "crucible-results", "mountPath": "/out"}])
        );
        assert_eq!(spec["initContainers"][0].get("volumeMounts"), None);
    }

    #[test]
    fn the_collectors_image_gets_the_pull_secret_its_registry_needs() {
        let cfg = Config {
            pull_secrets: vec![PullSecret::parse("quay-pull=quay.io/crucible").unwrap()],
            ..collecting_cfg()
        };
        let n = planned(bench(), "/out", &cfg).unwrap();
        assert_eq!(
            n.object.pointer(&format!("{POD_SPEC}/imagePullSecrets")),
            Some(&json!([{"name": "quay-pull"}]))
        );
        assert_eq!(n.server_changes.len(), 2);
        assert!(
            n.server_changes[1].contains("added quay-pull"),
            "{:?}",
            n.server_changes
        );
    }

    #[test]
    fn names_and_mounts_the_collector_takes_are_refused() {
        let cfg = collecting_cfg();
        let mut volume = bench();
        volume["spec"]["template"]["spec"]["volumes"] =
            json!([{"name": "crucible-results", "emptyDir": {}}]);
        let mut named = bench();
        named["spec"]["template"]["spec"]["containers"][0]["name"] = json!("crucible-results");
        let mut init = bench();
        init["spec"]["template"]["spec"]["initContainers"] =
            json!([{"name": "crucible-results", "image": "ubi9"}]);
        let mut mounted = bench();
        mounted["spec"]["template"]["spec"]["volumes"] = json!([{"name": "v", "emptyDir": {}}]);
        mounted["spec"]["template"]["spec"]["containers"][0]["volumeMounts"] =
            json!([{"name": "v", "mountPath": "/out/"}]);
        for (manifest, path) in [
            (volume, "/spec/template/spec/volumes/0/name"),
            (named, "/spec/template/spec/containers/0/name"),
            (init, "/spec/template/spec/initContainers/0/name"),
            (
                mounted,
                "/spec/template/spec/containers/0/volumeMounts/0/mountPath",
            ),
        ] {
            match planned(manifest, "/out", &cfg) {
                Err(ManifestError::Forbidden { path: got, .. }) => assert_eq!(got, path),
                other => panic!("{path}: {other:?}"),
            }
        }
        assert_eq!(
            planned(bench(), "/out", &cfg)
                .and_then(|n| planned(n.object, "/out", &cfg))
                .unwrap_err()
                .to_string(),
            "/spec/template/spec/volumes/0/name: the name crucible-results, which the results collector takes, is not allowed"
        );
    }

    #[test]
    fn results_take_a_job_on_a_server_with_a_collector() {
        let deployment = json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": {"name": "serve"},
            "spec": {"template": {"spec": {"containers": [{"name": "m", "image": "ubi9"}]}}},
        });
        assert_eq!(
            planned(deployment, "/out", &collecting_cfg()).unwrap_err(),
            ManifestError::ResultsKind { kind: "Deployment" }
        );
        assert_eq!(
            planned(bench(), "/out", &Config::default()).unwrap_err(),
            ManifestError::NoCollector
        );
    }

    #[test]
    fn sidecars_run_from_kubernetes_1_29() {
        for (major, minor) in [("1", "29"), ("1", "33+"), ("2", "0")] {
            assert!(runs_sidecars(major, minor), "{major}.{minor}");
        }
        for (major, minor) in [("1", "28"), ("1", "28+"), ("1", ""), ("", "30")] {
            assert!(!runs_sidecars(major, minor), "{major}.{minor}");
        }
    }

    #[test]
    fn a_results_directory_is_an_absolute_path_below_the_root() {
        for (raw, want) in [
            ("/out", "/out"),
            ("/work/out/", "/work/out"),
            ("//work//out", "/work/out"),
        ] {
            assert_eq!(ResultsDir::parse(raw).unwrap().as_str(), want);
        }
        for bad in ["", "/", "out", "./out", "/work/../etc", "/work/./out/.."] {
            assert_eq!(
                ResultsDir::parse(bad),
                Err(ManifestError::ResultsDir {
                    got: bad.to_string()
                }),
                "{bad}"
            );
        }
    }
}
