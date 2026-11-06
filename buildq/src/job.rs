//! The Job one build runs as: an init container exports the context image into the workspace,
//! then rootless buildah builds it, pushes the image, and writes its digest as the termination
//! message. The Job carries the Kueue queue label, so Kueue admits it against the queue's quota,
//! and Kubernetes deletes it a while after it ends.

use std::collections::BTreeMap;

use k8s_openapi::ByteString;
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1 as core;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};

pub const LABEL_MANAGED_BY: &str = "app.kubernetes.io/managed-by";
pub const MANAGED_BY: &str = "buildq";
pub const LABEL_BUILD: &str = "buildq.dev/build";
const LABEL_QUEUE: &str = "kueue.x-k8s.io/queue-name";
pub const BUILDER: &str = "builder";
const STORAGE: &str = "/home/build/.local/share/containers";

/// The containers-storage driver of the build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Storage {
    Overlay,
    Vfs,
}

impl Storage {
    fn as_str(self) -> &'static str {
        match self {
            Self::Overlay => "overlay",
            Self::Vfs => "vfs",
        }
    }
}

/// Where and how builds run, the same for every build of one server.
#[derive(Debug, Clone)]
pub struct Profile {
    pub namespace: String,
    pub queue: Option<String>,
    pub storage: Storage,
    pub arch: Option<String>,
    pub requests: BTreeMap<String, String>,
    pub deadline_secs: i64,
    pub ttl_secs: i32,
}

/// One build.
pub struct Build<'a> {
    pub id: &'a str,
    pub ctx_ref: &'a str,
    pub image: &'a str,
    pub dockerfile: &'a str,
    pub build_args: &'a BTreeMap<String, String>,
}

pub fn job(profile: &Profile, build: &Build<'_>) -> Job {
    let mut labels = BTreeMap::from([
        (LABEL_MANAGED_BY.to_string(), MANAGED_BY.to_string()),
        (LABEL_BUILD.to_string(), build.id.to_string()),
    ]);
    let pod_labels = labels.clone();
    if let Some(queue) = &profile.queue {
        labels.insert(LABEL_QUEUE.to_string(), queue.clone());
    }
    let args: String = build
        .build_args
        .iter()
        .map(|(k, v)| format!(" --build-arg {}", quote(&format!("{k}={v}"))))
        .collect();
    let script = format!(
        "mkdir -p /tmp/containers-run \
         && printf '[storage]\\ndriver = \"{driver}\"\\ngraphroot = \"{STORAGE}/storage\"\\nrunroot = \"/tmp/containers-run\"\\n' > /tmp/storage.conf \
         && buildah bud --layers --authfile /auth/config.json{args} -f {df} -t {img} /workspace \
         && buildah push --authfile /auth/config.json --digestfile /dev/termination-log {img} docker://{img}",
        driver = profile.storage.as_str(),
        df = quote(&format!("/workspace/{}", build.dockerfile)),
        img = quote(build.image),
    );
    let builder = core::Container {
        name: BUILDER.to_string(),
        image: Some("quay.io/buildah/stable:latest".to_string()),
        command: Some(vec!["/bin/sh".to_string(), "-c".to_string(), script]),
        env: Some(vec![
            env("BUILDAH_ISOLATION", "chroot"),
            env("CONTAINERS_STORAGE_CONF", "/tmp/storage.conf"),
            env("HOME", "/home/build"),
        ]),
        security_context: Some(run_as_1000()),
        resources: (!profile.requests.is_empty()).then(|| core::ResourceRequirements {
            requests: Some(
                profile
                    .requests
                    .iter()
                    .map(|(k, v)| (k.clone(), Quantity(v.clone())))
                    .collect(),
            ),
            ..Default::default()
        }),
        termination_message_policy: Some("FallbackToLogsOnError".to_string()),
        volume_mounts: Some(vec![
            mount("workspace", "/workspace", false),
            mount("auth", "/auth", true),
            mount("storage", STORAGE, false),
        ]),
        ..Default::default()
    };
    let fetch = core::Container {
        name: "fetch-context".to_string(),
        image: Some("gcr.io/go-containerregistry/crane:debug".to_string()),
        command: Some(vec![
            "/busybox/sh".to_string(),
            "-c".to_string(),
            "crane export \"$CTX_REF\" - | tar -x -C /workspace".to_string(),
        ]),
        env: Some(vec![
            env("DOCKER_CONFIG", "/auth"),
            env("CTX_REF", build.ctx_ref),
        ]),
        volume_mounts: Some(vec![
            mount("workspace", "/workspace", false),
            mount("auth", "/auth", true),
        ]),
        ..Default::default()
    };
    Job {
        metadata: ObjectMeta {
            name: Some(build.id.to_string()),
            namespace: Some(profile.namespace.clone()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(JobSpec {
            backoff_limit: Some(0),
            suspend: profile.queue.as_ref().map(|_| true),
            ttl_seconds_after_finished: Some(profile.ttl_secs),
            active_deadline_seconds: Some(profile.deadline_secs),
            template: core::PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(pod_labels),
                    ..Default::default()
                }),
                spec: Some(core::PodSpec {
                    restart_policy: Some("Never".to_string()),
                    automount_service_account_token: Some(false),
                    enable_service_links: Some(false),
                    security_context: Some(core::PodSecurityContext {
                        fs_group: Some(1000),
                        ..Default::default()
                    }),
                    node_selector: profile.arch.as_ref().map(|arch| {
                        BTreeMap::from([("kubernetes.io/arch".to_string(), arch.clone())])
                    }),
                    init_containers: Some(vec![fetch]),
                    containers: vec![builder],
                    volumes: Some(vec![
                        empty_dir("workspace"),
                        empty_dir("storage"),
                        core::Volume {
                            name: "auth".to_string(),
                            secret: Some(core::SecretVolumeSource {
                                secret_name: Some(build.id.to_string()),
                                items: Some(vec![core::KeyToPath {
                                    key: ".dockerconfigjson".to_string(),
                                    path: "config.json".to_string(),
                                    ..Default::default()
                                }]),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The registry credential the build's pod mounts, owned by its Job so it goes with the Job.
pub fn secret(namespace: &str, id: &str, job_uid: &str, authfile: &[u8]) -> core::Secret {
    core::Secret {
        metadata: ObjectMeta {
            name: Some(id.to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(BTreeMap::from([(
                LABEL_MANAGED_BY.to_string(),
                MANAGED_BY.to_string(),
            )])),
            owner_references: Some(vec![OwnerReference {
                api_version: "batch/v1".to_string(),
                kind: "Job".to_string(),
                name: id.to_string(),
                uid: job_uid.to_string(),
                ..Default::default()
            }]),
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

fn run_as_1000() -> core::SecurityContext {
    core::SecurityContext {
        run_as_user: Some(1000),
        run_as_group: Some(1000),
        ..Default::default()
    }
}

fn env(name: &str, value: &str) -> core::EnvVar {
    core::EnvVar {
        name: name.to_string(),
        value: Some(value.to_string()),
        ..Default::default()
    }
}

fn mount(name: &str, path: &str, read_only: bool) -> core::VolumeMount {
    core::VolumeMount {
        name: name.to_string(),
        mount_path: path.to_string(),
        read_only: read_only.then_some(true),
        ..Default::default()
    }
}

fn empty_dir(name: &str) -> core::Volume {
    core::Volume {
        name: name.to_string(),
        empty_dir: Some(core::EmptyDirVolumeSource::default()),
        ..Default::default()
    }
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::job::{Build, Profile, Storage, job, secret};

    fn profile(queue: Option<&str>) -> Profile {
        Profile {
            namespace: "builds".to_string(),
            queue: queue.map(str::to_string),
            storage: Storage::Overlay,
            arch: Some("amd64".to_string()),
            requests: BTreeMap::from([("cpu".to_string(), "8".to_string())]),
            deadline_secs: 3600,
            ttl_secs: 900,
        }
    }

    #[test]
    fn a_queued_build_starts_suspended_for_kueue_and_ends_on_its_own() {
        let args = BTreeMap::from([("BASE".to_string(), "it's".to_string())]);
        let build = Build {
            id: "bq-1",
            ctx_ref: "quay.io/acme/b:ctx-1",
            image: "quay.io/acme/b:bq-1",
            dockerfile: "Dockerfile",
            build_args: &args,
        };
        let queued = serde_json::to_value(job(&profile(Some("builds")), &build)).unwrap();
        assert_eq!(
            queued["metadata"]["labels"]["kueue.x-k8s.io/queue-name"],
            "builds"
        );
        assert_eq!(queued["spec"]["suspend"], true);
        assert_eq!(queued["spec"]["backoffLimit"], 0);
        assert_eq!(queued["spec"]["ttlSecondsAfterFinished"], 900);
        assert_eq!(queued["spec"]["activeDeadlineSeconds"], 3600);
        let pod = &queued["spec"]["template"]["spec"];
        assert_eq!(pod["restartPolicy"], "Never");
        assert_eq!(pod["nodeSelector"]["kubernetes.io/arch"], "amd64");
        assert_eq!(pod["volumes"][2]["secret"]["secretName"], "bq-1");
        let script = pod["containers"][0]["command"][2].as_str().unwrap();
        assert!(script.contains("driver = \"overlay\""), "{script}");
        assert!(script.contains("--build-arg 'BASE=it'\\''s'"), "{script}");
        assert!(
            script.contains("--digestfile /dev/termination-log"),
            "{script}"
        );
        assert_eq!(pod["containers"][0]["resources"]["requests"]["cpu"], "8");
        assert_eq!(
            pod["initContainers"][0]["env"][1]["value"],
            "quay.io/acme/b:ctx-1"
        );

        let direct = serde_json::to_value(job(&profile(None), &build)).unwrap();
        assert!(direct["spec"].get("suspend").is_none());
        assert!(
            direct["metadata"]["labels"]
                .get("kueue.x-k8s.io/queue-name")
                .is_none()
        );

        let s = serde_json::to_value(secret("builds", "bq-1", "uid-1", b"{}")).unwrap();
        assert_eq!(s["metadata"]["ownerReferences"][0]["uid"], "uid-1");
        assert_eq!(s["type"], "kubernetes.io/dockerconfigjson");
    }
}
