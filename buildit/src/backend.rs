#![allow(clippy::disallowed_macros)]

use anyhow::{Context, Result, anyhow};
use clap::ValueEnum;
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{self as core, Pod};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use std::collections::BTreeMap;

#[derive(Clone, Copy)]
pub enum CacheVolume<'a> {
    Pvc(&'a str),
    // node-local NVMe, the coreweave speed play; pair with PodOpts.node
    HostPath(&'a str),
}

pub struct PodOpts<'a> {
    pub idle_nodes: &'a [String],
    pub resources: &'a Resources,
    pub cache: Option<CacheVolume<'a>>,
    pub node: Option<&'a str>,
}

#[derive(Default)]
pub struct Resources {
    pub requests: Vec<(String, String)>,
    pub limits: Vec<(String, String)>,
}

impl Resources {
    fn requirements(&self) -> Option<core::ResourceRequirements> {
        let map = |kvs: &[(String, String)]| {
            (!kvs.is_empty()).then(|| {
                kvs.iter()
                    .map(|(k, v)| (k.clone(), Quantity(v.clone())))
                    .collect::<BTreeMap<_, _>>()
            })
        };
        let requests = map(&self.requests);
        let limits = map(&self.limits);
        (requests.is_some() || limits.is_some()).then(|| core::ResourceRequirements {
            requests,
            limits,
            ..Default::default()
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum Backend {
    /// rootless buildkitd, driven one-shot via buildctl-daemonless.sh
    Buildkit,
    /// kaniko executor (unmaintained upstream, but a known-good fallback)
    Kaniko,
    /// rootless buildah, chroot isolation + vfs storage
    Buildah,
}

impl Backend {
    pub fn shell(&self) -> &'static str {
        match self {
            Backend::Buildkit => "/bin/sh",
            Backend::Kaniko => "/busybox/sh",
            Backend::Buildah => "/bin/sh",
        }
    }

    // rootless backends are uid 1000 and can't mkdir at /, so: $HOME
    pub fn workspace(&self) -> &'static str {
        match self {
            Backend::Buildkit => "/home/user/workspace",
            Backend::Kaniko => "/workspace",
            Backend::Buildah => "/home/build/workspace",
        }
    }

    fn auth_path(&self) -> &'static str {
        match self {
            Backend::Buildkit => "/home/user/.docker/config.json",
            Backend::Kaniko => "/kaniko/.docker/config.json",
            Backend::Buildah => "/tmp/auth.json",
        }
    }

    fn digest_path(&self) -> &'static str {
        match self {
            // buildctl writes a JSON metadata file, not a bare digest
            Backend::Buildkit => "/tmp/metadata.json",
            Backend::Kaniko => "/tmp/digest",
            Backend::Buildah => "/tmp/digest",
        }
    }

    pub fn digest_command(&self) -> Vec<String> {
        vec![
            self.shell().to_string(),
            "-c".to_string(),
            format!("cat {}", self.digest_path()),
        ]
    }

    pub fn digest_from(&self, raw: &str) -> Result<String> {
        let digest = match self {
            Backend::Buildkit => {
                let meta: serde_json::Value =
                    serde_json::from_str(raw).context("parsing buildkit metadata file")?;
                meta.get("containerimage.digest")
                    .and_then(|d| d.as_str())
                    .ok_or_else(|| anyhow!("no containerimage.digest in buildkit metadata"))?
                    .to_string()
            }
            Backend::Kaniko | Backend::Buildah => raw.trim().to_string(),
        };
        if !digest.starts_with("sha256:") {
            return Err(anyhow!("unexpected digest: {digest:?}"));
        }
        Ok(digest)
    }

    pub fn setup_command(&self) -> Vec<String> {
        let ws = self.workspace();
        let script = match self {
            Backend::Buildkit => format!("mkdir -p {ws} /home/user/.docker"),
            // kaniko:debug ships without /tmp, which breaks anything tar-shaped
            Backend::Kaniko => format!("mkdir -p {ws} /tmp /kaniko/.docker"),
            // the stable image's storage.conf points at a rootful store under
            // /usr/lib that rootless vfs can't lock, so bring our own conf
            Backend::Buildah => format!(
                "mkdir -p {ws} /tmp/containers-run && printf '[storage]\\ndriver = \"vfs\"\\ngraphroot = \"/home/build/.local/share/containers/storage\"\\nrunroot = \"/tmp/containers-run\"\\n' > /tmp/storage.conf"
            ),
        };
        vec![self.shell().to_string(), "-c".to_string(), script]
    }

    pub fn untar_command(&self) -> Vec<String> {
        vec![
            self.shell().to_string(),
            "-c".to_string(),
            format!("tar xzf - -C {}", self.workspace()),
        ]
    }

    pub fn auth_upload_command(&self) -> Vec<String> {
        vec![
            self.shell().to_string(),
            "-c".to_string(),
            format!("cat > {}", self.auth_path()),
        ]
    }

    // each step is plain argv, no shell, no quoting games
    pub fn build_steps(
        &self,
        image: &str,
        dockerfile: &str,
        target: Option<&str>,
        push: bool,
        build_args: &[String],
        labels: &[(String, String)],
    ) -> Vec<Vec<String>> {
        let ws = self.workspace();
        match self {
            Backend::Buildkit => {
                let mut build = vec![
                    "buildctl-daemonless.sh".to_string(),
                    "build".to_string(),
                    "--frontend".to_string(),
                    "dockerfile.v0".to_string(),
                    "--local".to_string(),
                    format!("context={ws}"),
                    "--local".to_string(),
                    format!("dockerfile={ws}"),
                    "--opt".to_string(),
                    format!("filename={dockerfile}"),
                    "--output".to_string(),
                    format!("type=image,name={image},push={push}"),
                    "--metadata-file".to_string(),
                    self.digest_path().to_string(),
                ];
                if let Some(t) = target {
                    build.push("--opt".to_string());
                    build.push(format!("target={t}"));
                }
                for arg in build_args {
                    build.push("--opt".to_string());
                    build.push(format!("build-arg:{arg}"));
                }
                for (k, v) in labels {
                    build.push("--opt".to_string());
                    build.push(format!("label:{k}={v}"));
                }
                vec![build]
            }
            Backend::Kaniko => {
                let mut exec = vec![
                    "/kaniko/executor".to_string(),
                    format!("--dockerfile={ws}/{dockerfile}"),
                    format!("--context=dir://{ws}"),
                    format!("--destination={image}"),
                    // no --cleanup: it nukes the fs, digest file and all
                    format!("--digest-file={}", self.digest_path()),
                ];
                if let Some(t) = target {
                    exec.push(format!("--target={t}"));
                }
                if !push {
                    exec.push("--no-push".to_string());
                }
                for arg in build_args {
                    exec.push(format!("--build-arg={arg}"));
                }
                for (k, v) in labels {
                    exec.push("--label".to_string());
                    exec.push(format!("{k}={v}"));
                }
                vec![exec]
            }
            // isolation/storage-driver flags live in the pod spec env
            Backend::Buildah => {
                let mut bud = vec![
                    "buildah".to_string(),
                    "bud".to_string(),
                    "--authfile".to_string(),
                    self.auth_path().to_string(),
                ];
                if let Some(t) = target {
                    bud.push("--target".to_string());
                    bud.push(t.to_string());
                }
                for arg in build_args {
                    bud.push("--build-arg".to_string());
                    bud.push(arg.clone());
                }
                for (k, v) in labels {
                    bud.push("--label".to_string());
                    bud.push(format!("{k}={v}"));
                }
                bud.extend([
                    "-f".to_string(),
                    format!("{ws}/{dockerfile}"),
                    "-t".to_string(),
                    image.to_string(),
                    ws.to_string(),
                ]);
                if !push {
                    return vec![bud];
                }
                let push = vec![
                    "buildah".to_string(),
                    "push".to_string(),
                    "--authfile".to_string(),
                    self.auth_path().to_string(),
                    "--digestfile".to_string(),
                    self.digest_path().to_string(),
                    image.to_string(),
                    format!("docker://{image}"),
                ];
                vec![bud, push]
            }
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Backend::Buildkit => "buildkit",
            Backend::Kaniko => "kaniko",
            Backend::Buildah => "buildah",
        }
    }

    // where a persistent cache volume pays off for each backend
    fn cache_mount_path(&self) -> &'static str {
        match self {
            Backend::Buildkit => "/home/user/.local/share/buildkit",
            // kaniko caches base images under --cache-dir
            Backend::Kaniko => "/cache",
            Backend::Buildah => "/home/build/.local/share/containers",
        }
    }

    // volume named for the mount it replaces; kaniko gets a dedicated one
    fn cache_volume_name(&self) -> &'static str {
        match self {
            Backend::Buildkit => "buildkit-storage",
            Backend::Kaniko => "kaniko-cache",
            Backend::Buildah => "containers-storage",
        }
    }

    pub fn pod_spec(&self, name: &str, namespace: &str, opts: &PodOpts<'_>) -> Pod {
        let mut container = self.pod_builder_container();
        container.resources = opts.resources.requirements();
        let mut volumes = self.storage_volumes();
        if let Some(cache) = opts.cache {
            self.apply_cache(&mut container, &mut volumes, cache);
        }
        Pod {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some(namespace.to_string()),
                labels: Some(self.labels()),
                annotations: Some(BTreeMap::from([apparmor_unconfined("builder")])),
                ..Default::default()
            },
            spec: Some(core::PodSpec {
                restart_policy: Some("Never".to_string()),
                active_deadline_seconds: Some(7200),
                containers: vec![container],
                volumes: Some(volumes),
                security_context: matches!(opts.cache, Some(CacheVolume::Pvc(_))).then(|| {
                    core::PodSecurityContext {
                        fs_group: Some(1000),
                        ..Default::default()
                    }
                }),
                init_containers: self.cache_perm_fix(opts.cache).map(|fix| vec![fix]),
                node_selector: opts.node.map(hostname_selector),
                affinity: preferred_nodes(opts.idle_nodes),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn labels(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("app".to_string(), "buildit".to_string()),
            ("buildit/backend".to_string(), self.label().to_string()),
        ])
    }

    /// The long-lived builder the interactive mode execs into.
    fn pod_builder_container(&self) -> core::Container {
        let sleep = Some(vec!["sleep".to_string(), "7200".to_string()]);
        match self {
            Backend::Buildkit => core::Container {
                name: "builder".to_string(),
                image: Some("moby/buildkit:rootless".to_string()),
                command: sleep,
                env: Some(vec![
                    env("BUILDKITD_FLAGS", "--oci-worker-no-process-sandbox"),
                    env("HOME", "/home/user"),
                ]),
                security_context: Some(run_as_1000(true)),
                volume_mounts: Some(vec![mount(
                    "buildkit-storage",
                    "/home/user/.local/share/buildkit",
                )]),
                ..Default::default()
            },
            Backend::Kaniko => core::Container {
                name: "builder".to_string(),
                image: Some("gcr.io/kaniko-project/executor:debug".to_string()),
                command: Some(vec![
                    "/busybox/sh".to_string(),
                    "-c".to_string(),
                    "sleep 7200".to_string(),
                ]),
                ..Default::default()
            },
            Backend::Buildah => core::Container {
                name: "builder".to_string(),
                image: Some("quay.io/buildah/stable:latest".to_string()),
                command: sleep,
                security_context: Some(run_as_1000(false)),
                env: Some(buildah_env()),
                volume_mounts: Some(vec![mount(
                    "containers-storage",
                    "/home/build/.local/share/containers",
                )]),
                ..Default::default()
            },
        }
    }

    /// The backend's own layer store, an emptyDir unless a cache volume replaces it.
    fn storage_volumes(&self) -> Vec<core::Volume> {
        match self {
            Backend::Buildkit => vec![empty_dir("buildkit-storage")],
            Backend::Kaniko => Vec::new(),
            Backend::Buildah => vec![empty_dir("containers-storage")],
        }
    }

    // point the backend's cache volume at the PVC or hostPath; for kaniko
    // also add the mount (it has no storage volume otherwise) and cache flags
    fn apply_cache(
        &self,
        container: &mut core::Container,
        volumes: &mut Vec<core::Volume>,
        cache: CacheVolume<'_>,
    ) {
        let vol_name = self.cache_volume_name();
        volumes.retain(|v| v.name != vol_name);
        volumes.push(match cache {
            CacheVolume::Pvc(name) => core::Volume {
                name: vol_name.to_string(),
                persistent_volume_claim: Some(core::PersistentVolumeClaimVolumeSource {
                    claim_name: name.to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            CacheVolume::HostPath(path) => core::Volume {
                name: vol_name.to_string(),
                host_path: Some(core::HostPathVolumeSource {
                    path: path.to_string(),
                    type_: Some("DirectoryOrCreate".to_string()),
                }),
                ..Default::default()
            },
        });
        if let Backend::Kaniko = self {
            container
                .volume_mounts
                .get_or_insert_with(Vec::new)
                .push(mount(vol_name, self.cache_mount_path()));
            // kaniko: cache-dir holds base images; layer cache goes to the
            // registry (push failures there are warnings, not errors)
            if let Some(cmd) = &mut container.command {
                cmd.push("--cache=true".to_string());
                cmd.push(format!("--cache-dir={}", self.cache_mount_path()));
            }
        }
    }

    // hostPath dirs come up root:root 0755 and fsGroup doesn't touch them,
    // so rootless builders need a one-shot chmod before they can write
    fn cache_perm_fix(&self, cache: Option<CacheVolume<'_>>) -> Option<core::Container> {
        match (cache, self) {
            (Some(CacheVolume::HostPath(_)), Backend::Buildkit | Backend::Buildah) => {
                Some(core::Container {
                    name: "fix-cache-perms".to_string(),
                    image: Some("busybox:1.36".to_string()),
                    command: Some(vec![
                        "chmod".to_string(),
                        "1777".to_string(),
                        self.cache_mount_path().to_string(),
                    ]),
                    security_context: Some(core::SecurityContext {
                        run_as_user: Some(0),
                        run_as_group: Some(0),
                        ..Default::default()
                    }),
                    volume_mounts: Some(vec![mount(
                        self.cache_volume_name(),
                        self.cache_mount_path(),
                    )]),
                    ..Default::default()
                })
            }
            _ => None,
        }
    }

    // job mode: build runs as the container command with context staged by an
    // initContainer, so the Job gets real completion/retry/TTL semantics. the
    // digest goes to /dev/termination-log, which k8s preserves in pod status.
    fn job_builder_container(
        &self,
        image: &str,
        dockerfile: &str,
        target: Option<&str>,
        build_args: &[String],
        labels: &[(String, String)],
    ) -> core::Container {
        let df = format!("/workspace/{dockerfile}");
        let mut container = match self {
            Backend::Buildkit => {
                let mut args: String = target
                    .map(|t| format!(" --opt {}", shell_quote(&format!("target={t}"))))
                    .into_iter()
                    .chain(
                        build_args
                            .iter()
                            .map(|a| format!(" --opt build-arg:{}", shell_quote(a))),
                    )
                    .collect();
                for (k, v) in labels {
                    args.push_str(&format!(
                        " --opt {}",
                        shell_quote(&format!("label:{k}={v}"))
                    ));
                }
                // metadata goes to /tmp first: buildctl writes it atomically
                // (tmp + rename) and you can't rename onto the bind-mounted
                // termination-log file, only write into it
                let script = format!(
                    "buildctl-daemonless.sh build --frontend dockerfile.v0 \
                     --local context=/workspace --local dockerfile=/workspace \
                     --opt filename={df} --output type=image,name={img},push=true \
                     --metadata-file /tmp/metadata.json{args} \
                     && cat /tmp/metadata.json > /dev/termination-log",
                    df = shell_quote(dockerfile),
                    img = shell_quote(image),
                );
                core::Container {
                    image: Some("moby/buildkit:rootless".to_string()),
                    command: Some(sh(script)),
                    env: Some(vec![
                        env("BUILDKITD_FLAGS", "--oci-worker-no-process-sandbox"),
                        env("HOME", "/home/user"),
                        env("DOCKER_CONFIG", "/auth"),
                    ]),
                    security_context: Some(run_as_1000(true)),
                    volume_mounts: Some(vec![mount(
                        "buildkit-storage",
                        "/home/user/.local/share/buildkit",
                    )]),
                    ..Default::default()
                }
            }
            Backend::Kaniko => {
                let mut command = vec![
                    "/kaniko/executor".to_string(),
                    format!("--dockerfile={df}"),
                    "--context=dir:///workspace".to_string(),
                    format!("--destination={image}"),
                    "--digest-file=/dev/termination-log".to_string(),
                ];
                if let Some(t) = target {
                    command.push(format!("--target={t}"));
                }
                for arg in build_args {
                    command.push(format!("--build-arg={arg}"));
                }
                for (k, v) in labels {
                    command.push(format!("--label={k}={v}"));
                }
                core::Container {
                    image: Some("gcr.io/kaniko-project/executor:debug".to_string()),
                    command: Some(command),
                    env: Some(vec![env("DOCKER_CONFIG", "/auth")]),
                    ..Default::default()
                }
            }
            Backend::Buildah => {
                let mut args: String = target
                    .map(|t| format!(" --target {}", shell_quote(t)))
                    .into_iter()
                    .chain(
                        build_args
                            .iter()
                            .map(|a| format!(" --build-arg {}", shell_quote(a))),
                    )
                    .collect();
                for (k, v) in labels {
                    args.push_str(&format!(" --label {}", shell_quote(&format!("{k}={v}"))));
                }
                let script = format!(
                    "printf '[storage]\\ndriver = \"vfs\"\\ngraphroot = \"/home/build/.local/share/containers/storage\"\\nrunroot = \"/tmp/containers-run\"\\n' > /tmp/storage.conf \
                     && mkdir -p /tmp/containers-run \
                     && buildah bud --authfile /auth/config.json{args} -f {df} -t {img} /workspace \
                     && buildah push --authfile /auth/config.json --digestfile /dev/termination-log {img} docker://{img}",
                    img = shell_quote(image),
                    df = shell_quote(&df),
                );
                core::Container {
                    image: Some("quay.io/buildah/stable:latest".to_string()),
                    command: Some(sh(script)),
                    env: Some(buildah_env()),
                    security_context: Some(run_as_1000(false)),
                    volume_mounts: Some(vec![mount(
                        "containers-storage",
                        "/home/build/.local/share/containers",
                    )]),
                    ..Default::default()
                }
            }
        };
        container.name = "builder".to_string();
        // carry failure logs in the message when there's no digest to report
        container.termination_message_policy = Some("FallbackToLogsOnError".to_string());
        let mut all = vec![
            mount("workspace", "/workspace"),
            read_only_mount("auth", "/auth"),
        ];
        all.extend(container.volume_mounts.take().unwrap_or_default());
        container.volume_mounts = Some(all);
        container
    }

    pub fn job_spec(&self, name: &str, namespace: &str, args: &crate::job::DetachArgs<'_>) -> Job {
        let mut builder = self.job_builder_container(
            args.image,
            args.dockerfile,
            args.target,
            args.build_args,
            args.labels,
        );
        builder.resources = args.resources.requirements();
        let mut backend_volumes = self.storage_volumes();
        if let Some(cache) = args.cache {
            self.apply_cache(&mut builder, &mut backend_volumes, cache);
        }
        let mut volumes = vec![
            empty_dir("workspace"),
            core::Volume {
                name: "auth".to_string(),
                secret: Some(core::SecretVolumeSource {
                    secret_name: Some(name.to_string()),
                    items: Some(vec![core::KeyToPath {
                        key: ".dockerconfigjson".to_string(),
                        path: "config.json".to_string(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ];
        volumes.extend(backend_volumes);
        let fetch_context = core::Container {
            name: "fetch-context".to_string(),
            image: Some("gcr.io/go-containerregistry/crane:debug".to_string()),
            command: Some(sh(
                "crane export \"$CTX_REF\" - | tar -x -C /workspace".to_string()
            )),
            env: Some(vec![
                env("DOCKER_CONFIG", "/auth"),
                env("CTX_REF", args.ctx_ref),
            ]),
            volume_mounts: Some(vec![
                mount("workspace", "/workspace"),
                read_only_mount("auth", "/auth"),
            ]),
            ..Default::default()
        };
        let mut init = vec![fetch_context];
        if let Some(fix) = self.cache_perm_fix(args.cache) {
            init.insert(0, fix);
        }
        Job {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some(namespace.to_string()),
                labels: Some(self.labels()),
                annotations: Some(BTreeMap::from([(
                    "buildit/image".to_string(),
                    args.image.to_string(),
                )])),
                ..Default::default()
            },
            spec: Some(JobSpec {
                // retries are safe: the context is content-addressed in the registry
                backoff_limit: Some(2),
                ttl_seconds_after_finished: Some(3600),
                active_deadline_seconds: Some(7200),
                template: core::PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        labels: Some(self.labels()),
                        annotations: Some(BTreeMap::from([
                            apparmor_unconfined("builder"),
                            apparmor_unconfined("fetch-context"),
                        ])),
                        ..Default::default()
                    }),
                    spec: Some(core::PodSpec {
                        restart_policy: Some("Never".to_string()),
                        // emptyDir is root:root 0755; fsGroup lets uid-1000 builders read/write it
                        security_context: Some(core::PodSecurityContext {
                            fs_group: Some(1000),
                            ..Default::default()
                        }),
                        init_containers: Some(init),
                        containers: vec![builder],
                        volumes: Some(volumes),
                        node_selector: args.node.map(hostname_selector),
                        affinity: preferred_nodes(args.idle_nodes),
                        ..Default::default()
                    }),
                },
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

fn env(name: &str, value: &str) -> core::EnvVar {
    core::EnvVar {
        name: name.to_string(),
        value: Some(value.to_string()),
        value_from: None,
    }
}

fn sh(script: String) -> Vec<String> {
    vec!["sh".to_string(), "-c".to_string(), script]
}

fn mount(name: &str, path: &str) -> core::VolumeMount {
    core::VolumeMount {
        name: name.to_string(),
        mount_path: path.to_string(),
        ..Default::default()
    }
}

fn read_only_mount(name: &str, path: &str) -> core::VolumeMount {
    core::VolumeMount {
        read_only: Some(true),
        ..mount(name, path)
    }
}

fn empty_dir(name: &str) -> core::Volume {
    core::Volume {
        name: name.to_string(),
        empty_dir: Some(core::EmptyDirVolumeSource::default()),
        ..Default::default()
    }
}

/// uid 1000 so a rootless builder gets its own userns for layer unpack; buildkit's rootlesskit
/// also needs syscalls the default seccomp profile masks.
fn run_as_1000(seccomp_unconfined: bool) -> core::SecurityContext {
    core::SecurityContext {
        run_as_user: Some(1000),
        run_as_group: Some(1000),
        seccomp_profile: seccomp_unconfined.then(|| core::SeccompProfile {
            type_: "Unconfined".to_string(),
            localhost_profile: None,
        }),
        ..Default::default()
    }
}

fn buildah_env() -> Vec<core::EnvVar> {
    vec![
        env("BUILDAH_ISOLATION", "chroot"),
        env("STORAGE_DRIVER", "vfs"),
        env("CONTAINERS_STORAGE_CONF", "/tmp/storage.conf"),
        env("HOME", "/home/build"),
    ]
}

// Ubuntu nodes' default AppArmor profile denies the mount syscalls rootless
// builders need ("remount /: permission denied" and friends). unconfined or bust.
fn apparmor_unconfined(container: &str) -> (String, String) {
    (
        format!("container.apparmor.security.beta.kubernetes.io/{container}"),
        "unconfined".to_string(),
    )
}

fn hostname_selector(node: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("kubernetes.io/hostname".to_string(), node.to_string())])
}

fn preferred_nodes(idle_nodes: &[String]) -> Option<core::Affinity> {
    (!idle_nodes.is_empty()).then(|| core::Affinity {
        node_affinity: Some(core::NodeAffinity {
            preferred_during_scheduling_ignored_during_execution: Some(vec![
                core::PreferredSchedulingTerm {
                    weight: 100,
                    preference: core::NodeSelectorTerm {
                        match_expressions: Some(vec![core::NodeSelectorRequirement {
                            key: "kubernetes.io/hostname".to_string(),
                            operator: "In".to_string(),
                            values: Some(idle_nodes.to_vec()),
                        }]),
                        ..Default::default()
                    },
                },
            ]),
            ..Default::default()
        }),
        ..Default::default()
    })
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use crate::backend::{Backend, Resources};

    #[test]
    fn buildkit_build_steps() {
        let steps = Backend::Buildkit.build_steps(
            "quay.io/acme/foo:tag",
            "Dockerfile.tap",
            None,
            true,
            &["FOO=bar".to_string()],
            &[("quay.expires-after".to_string(), "1d".to_string())],
        );
        assert_eq!(steps.len(), 1);
        let argv = &steps[0];
        assert_eq!(argv[0], "buildctl-daemonless.sh");
        assert!(argv.contains(&"filename=Dockerfile.tap".to_string()));
        assert!(argv.contains(&"type=image,name=quay.io/acme/foo:tag,push=true".to_string()));
        assert!(argv.contains(&"build-arg:FOO=bar".to_string()));
        assert!(argv.contains(&"label:quay.expires-after=1d".to_string()));
    }

    #[test]
    fn kaniko_build_steps() {
        let steps = Backend::Kaniko.build_steps(
            "quay.io/acme/foo:tag",
            "Dockerfile.tap",
            None,
            true,
            &["FOO=bar".to_string()],
            &[("team".to_string(), "infra".to_string())],
        );
        assert_eq!(steps.len(), 1);
        let argv = &steps[0];
        assert_eq!(argv[0], "/kaniko/executor");
        assert!(argv.contains(&"--dockerfile=/workspace/Dockerfile.tap".to_string()));
        assert!(argv.contains(&"--destination=quay.io/acme/foo:tag".to_string()));
        assert!(argv.contains(&"--digest-file=/tmp/digest".to_string()));
        assert!(argv.contains(&"--build-arg=FOO=bar".to_string()));
        assert!(argv.contains(&"--label".to_string()) || argv.contains(&"team=infra".to_string()));
    }

    #[test]
    fn buildah_build_steps() {
        let steps = Backend::Buildah.build_steps(
            "quay.io/acme/foo:tag",
            "Dockerfile",
            None,
            true,
            &[],
            &[("team".to_string(), "infra".to_string())],
        );
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0][..2], ["buildah".to_string(), "bud".to_string()]);
        assert_eq!(steps[1][..2], ["buildah".to_string(), "push".to_string()]);
        assert_eq!(
            steps[1].last().map(String::as_str),
            Some("docker://quay.io/acme/foo:tag")
        );
        assert!(steps[1].contains(&"--digestfile".to_string()));
        assert!(steps[0].contains(&"team=infra".to_string()));
    }

    #[test]
    fn build_steps_stop_at_the_target_and_skip_the_push() {
        let steps = |backend: Backend, target, push| {
            backend.build_steps("quay.io/acme/foo:tag", "Dockerfile", target, push, &[], &[])
        };
        for backend in [Backend::Buildkit, Backend::Kaniko, Backend::Buildah] {
            let all = steps(backend, None, true).concat().join(" ");
            assert!(!all.contains("target"), "{backend:?}: {all}");
            assert!(!all.contains("--no-push"), "{backend:?}: {all}");
        }

        let buildkit = steps(Backend::Buildkit, Some("base"), false);
        assert_eq!(buildkit.len(), 1);
        let argv = &buildkit[0];
        let opt = argv.iter().position(|a| a == "target=base").unwrap();
        assert_eq!(argv[opt - 1], "--opt");
        assert!(argv.contains(&"type=image,name=quay.io/acme/foo:tag,push=false".to_string()));
        assert!(!argv.iter().any(|a| a.contains("push=true")), "{argv:?}");

        let kaniko = steps(Backend::Kaniko, Some("base"), false);
        assert_eq!(kaniko.len(), 1);
        assert!(kaniko[0].contains(&"--target=base".to_string()));
        assert!(kaniko[0].contains(&"--no-push".to_string()));
        let pushed = steps(Backend::Kaniko, Some("base"), true);
        assert!(!pushed[0].contains(&"--no-push".to_string()));

        let buildah = steps(Backend::Buildah, Some("base"), false);
        assert_eq!(buildah.len(), 1, "no push step: {buildah:?}");
        let bud = &buildah[0];
        assert_eq!(bud[..2], ["buildah".to_string(), "bud".to_string()]);
        let t = bud.iter().position(|a| a == "--target").unwrap();
        assert_eq!(bud[t + 1], "base");
        let pushed = steps(Backend::Buildah, Some("base"), true);
        assert_eq!(pushed.len(), 2);
        assert!(pushed[0].contains(&"--target".to_string()));
    }

    #[test]
    fn job_builds_stop_at_the_target() {
        for (backend, want) in [
            (Backend::Buildkit, "--opt 'target=base'"),
            (Backend::Kaniko, "--target=base"),
            (Backend::Buildah, "--target 'base'"),
        ] {
            let resources = Resources::default();
            let args = |target| crate::job::DetachArgs {
                image: "quay.io/acme/foo:tag",
                dockerfile: "Dockerfile",
                target,
                build_args: &[],
                ctx_ref: "quay.io/acme/foo:buildit-ctx-deadbeef0123",
                authfile: b"",
                idle_nodes: &[],
                resources: &resources,
                labels: &[],
                cache: None,
                node: None,
            };
            let cmd = |target| {
                let job = backend.job_spec("buildit-abc123", "builds", &args(target));
                job.spec.unwrap().template.spec.unwrap().containers[0]
                    .command
                    .as_deref()
                    .unwrap_or_default()
                    .join(" ")
            };
            assert!(
                cmd(Some("base")).contains(want),
                "{backend:?}: {}",
                cmd(Some("base"))
            );
            assert!(!cmd(None).contains("target"), "{backend:?}: {}", cmd(None));
        }
    }

    #[test]
    fn shell_quote_survives_hostile_input() {
        assert_eq!(crate::backend::shell_quote("plain"), "'plain'");
        assert_eq!(
            crate::backend::shell_quote("a'b; rm -rf /"),
            r#"'a'\''b; rm -rf /'"#
        );
    }

    #[test]
    fn job_specs_deserialize() {
        let resources = crate::backend::Resources {
            requests: vec![("cpu".to_string(), "2".to_string())],
            limits: vec![("memory".to_string(), "8Gi".to_string())],
        };
        for backend in [Backend::Buildkit, Backend::Buildah, Backend::Kaniko] {
            let job = backend.job_spec(
                "buildit-abc123",
                "builds",
                &crate::job::DetachArgs {
                    image: "quay.io/acme/foo:tag",
                    dockerfile: "Dockerfile",
                    target: None,
                    build_args: &["FOO=bar".to_string()],
                    ctx_ref: "quay.io/acme/foo:buildit-ctx-deadbeef0123",
                    authfile: b"",
                    idle_nodes: &["node-a".to_string()],
                    resources: &resources,
                    labels: &[("team".to_string(), "infra".to_string())],
                    cache: Some(crate::backend::CacheVolume::HostPath("/mnt/nvme/c")),
                    node: Some("nvme-node-1"),
                },
            );
            let spec = job.spec.unwrap();
            assert_eq!(spec.backoff_limit, Some(2));
            assert_eq!(spec.ttl_seconds_after_finished, Some(3600));
            let pod_spec = spec.template.spec.unwrap();
            assert!(pod_spec.affinity.is_some());
            assert_eq!(
                pod_spec.security_context.unwrap().fs_group,
                Some(1000),
                "uid-1000 builders need the emptyDir group-writable"
            );
            let inits = pod_spec.init_containers.unwrap();
            assert!(inits.iter().any(|c| c.name == "fetch-context"));
            assert_eq!(
                inits.iter().any(|c| c.name == "fix-cache-perms"),
                backend != Backend::Kaniko,
                "{backend:?}"
            );
            assert_eq!(
                pod_spec.node_selector.as_ref().unwrap()["kubernetes.io/hostname"],
                "nvme-node-1"
            );
            let builder = &pod_spec.containers[0];
            assert_eq!(builder.name, "builder");
            assert_eq!(
                builder.termination_message_policy.as_deref(),
                Some("FallbackToLogsOnError")
            );
            let mounts: Vec<_> = builder
                .volume_mounts
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|m| m.mount_path.as_str())
                .collect();
            assert!(mounts.contains(&"/workspace"), "{mounts:?}");
            assert!(mounts.contains(&"/auth"), "{mounts:?}");
            let cmd = builder.command.as_deref().unwrap_or_default().join(" ");
            assert!(cmd.contains("/dev/termination-log"), "{cmd}");
            let res = builder.resources.as_ref().unwrap();
            assert_eq!(res.requests.as_ref().unwrap()["cpu"].0, "2");
            assert_eq!(res.limits.as_ref().unwrap()["memory"].0, "8Gi");
            let annotations = job.metadata.annotations.unwrap();
            assert_eq!(
                annotations.get("buildit/image").map(String::as_str),
                Some("quay.io/acme/foo:tag")
            );
        }
    }

    #[test]
    fn digest_parsing() {
        assert_eq!(
            Backend::Kaniko.digest_from("sha256:abc\n").unwrap(),
            "sha256:abc"
        );
        let meta = r#"{"containerimage.digest":"sha256:def","image.name":"x"}"#;
        assert_eq!(Backend::Buildkit.digest_from(meta).unwrap(), "sha256:def");
        assert!(Backend::Buildkit.digest_from("{}").is_err());
        assert!(Backend::Kaniko.digest_from("garbage").is_err());
    }

    fn opts<'a>(idle_nodes: &'a [String], resources: &'a Resources) -> crate::backend::PodOpts<'a> {
        crate::backend::PodOpts {
            idle_nodes,
            resources,
            cache: None,
            node: None,
        }
    }

    #[test]
    fn pod_spec_cache_and_node_pinning() {
        let empty = Resources::default();
        for backend in [Backend::Buildkit, Backend::Buildah, Backend::Kaniko] {
            let pvc_opts = crate::backend::PodOpts {
                cache: Some(crate::backend::CacheVolume::Pvc("build-cache")),
                node: Some("nvme-node-1"),
                ..opts(&[], &empty)
            };
            let pod = backend.pod_spec("buildit-abc123", "builds", &pvc_opts);
            let spec = pod.spec.unwrap();
            assert_eq!(
                spec.node_selector.as_ref().unwrap()["kubernetes.io/hostname"],
                "nvme-node-1"
            );
            let vols = spec.volumes.unwrap();
            assert!(
                vols.iter().any(|v| v
                    .persistent_volume_claim
                    .as_ref()
                    .is_some_and(|c| c.claim_name == "build-cache")),
                "{backend:?} missing pvc volume"
            );

            let hp_opts = crate::backend::PodOpts {
                cache: Some(crate::backend::CacheVolume::HostPath("/mnt/nvme/cache")),
                ..opts(&[], &empty)
            };
            let pod = backend.pod_spec("buildit-abc123", "builds", &hp_opts);
            let spec = pod.spec.unwrap();
            let vols = spec.volumes.unwrap();
            assert!(
                vols.iter().any(|v| v
                    .host_path
                    .as_ref()
                    .is_some_and(|h| h.path == "/mnt/nvme/cache")),
                "{backend:?} missing hostPath volume"
            );
            // rootless backends need the chmod init; kaniko runs as root
            let has_fix = spec
                .init_containers
                .as_deref()
                .unwrap_or_default()
                .iter()
                .any(|c| c.name == "fix-cache-perms");
            assert_eq!(has_fix, backend != Backend::Kaniko, "{backend:?}");
            if backend == Backend::Kaniko {
                let cmd = spec.containers[0].command.as_deref().unwrap_or_default();
                assert!(cmd.iter().any(|a| a == "--cache=true"), "{cmd:?}");
            }
        }
    }

    #[test]
    fn pod_specs_deserialize_with_and_without_affinity() {
        let cpu2 = crate::backend::Resources {
            requests: vec![("cpu".to_string(), "2".to_string())],
            limits: vec![],
        };
        for backend in [Backend::Buildkit, Backend::Buildah, Backend::Kaniko] {
            let pod = backend.pod_spec("buildit-abc123", "builds", &opts(&[], &cpu2));
            let res = pod.spec.as_ref().unwrap().containers[0]
                .resources
                .as_ref()
                .unwrap();
            assert_eq!(res.requests.as_ref().unwrap()["cpu"].0, "2");
            assert!(res.limits.is_none());
            let spec = pod.spec.unwrap();
            assert_eq!(spec.active_deadline_seconds, Some(7200));
            assert_eq!(spec.restart_policy.as_deref(), Some("Never"));
            assert!(spec.affinity.is_none());
            let labels = pod.metadata.labels.unwrap();
            assert_eq!(labels.get("app").map(String::as_str), Some("buildit"));

            let idle = vec!["node-a".to_string(), "node-b".to_string()];
            let none = Resources::default();
            let pod = backend.pod_spec("buildit-abc123", "builds", &opts(&idle, &none));
            let affinity = pod.spec.unwrap().affinity.unwrap();
            let prefs = affinity
                .node_affinity
                .unwrap()
                .preferred_during_scheduling_ignored_during_execution
                .unwrap();
            assert_eq!(prefs.len(), 1);
            let expr = &prefs[0].preference.match_expressions.as_ref().unwrap()[0];
            assert_eq!(expr.key, "kubernetes.io/hostname");
            assert_eq!(expr.values.as_ref().unwrap(), &idle);
        }
    }
}
