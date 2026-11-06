//! The Kubernetes Jobs `buildit mcp` runs its builds, runs and pushes as. Each is one pod that
//! Kueue admits, runs in a user namespace, and ends; Kubernetes deletes the Job a while later.
//!
//! ```text
//!  shared cache                                         cluster registry
//!  ctx/<hex> ─────────fetch─────────▶ ┌─ build pod ───────┐ ─push─▶ <repo>:<sandbox>-<id>
//!                                     │ builder: bud      │               │
//!  builds/<sandbox>/<id>/ ◀─archive───│ archive: sidecar  │               │ kubelet pulls
//!    facts.tar image build.exit       └───────────────────┘               ▼
//!    run-<n>.tar run-<n>.exit ◀─archive──────────────────── ┌─ run pod ─────────┐
//!                                                           │ run: cmd in image │
//!                                                           │ archive: sidecar  │
//!                                                           └───────────────────┘
//! ```
//!
//! Logs go to the pod's stdout, which `buildit mcp` keeps on its own disk once the Job ends. The
//! archive container is a native sidecar: the kubelet stops it only after the main container has
//! exited, however it exited, so a pod's results reach the shared cache even when the build
//! fails, times out or is cancelled. Its exit record goes up last, so its presence means
//! the rest is there. The write token is mounted into the archive container alone.

use std::collections::BTreeMap;

use k8s_openapi::ByteString;
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1 as core;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};

use crate::backend::{Resources, StorageDriver};
use crate::step::State;

pub const BUILDER: &str = "builder";
pub const RUN_CONTAINER: &str = "run";
pub const LABEL_KIND: &str = "buildit.dev/kind";
pub const LABEL_RUN: &str = "buildit.dev/run";
pub const ANNOTATION_CANCELLED: &str = "buildit.dev/cancelled";
const LABEL_QUEUE: &str = "kueue.x-k8s.io/queue-name";

const BUILDAH_IMAGE: &str = "quay.io/buildah/stable:latest";
const TOOLS_IMAGE: &str = "gcr.io/go-containerregistry/crane:debug";
pub const WORKSPACE: &str = "/workspace";
pub const OUT: &str = "/buildit-out";
pub const FACTS: &str = "/buildit-out/facts";
const TOOLS: &str = "/buildit-tools";
const INPUTS: &str = "/buildit-inputs";
const ARCHIVE_AUTH: &str = "/buildit-archive";
const CLUSTER: &str = "/buildit-cluster";
const STORAGE: &str = "/home/build/.local/share/containers";
/// What a pod may leave for the archive: facts, a run's log and the fetched paths `buildit mcp`
/// takes up to `step::FETCH_LIMIT` of.
const OUT_LIMIT: &str = "2Gi";
const FACTS_LIMIT: &str = "256Mi";
const TOOLS_LIMIT: &str = "64Mi";
/// Time the archive sidecar has to upload once the main container exits; each upload takes at
/// most 120s.
const GRACE_SECS: i64 = 600;

/// What a Job does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Build,
    Run,
    Push,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Run => "run",
            Self::Push => "push",
        }
    }
}

/// How build pods reach the registry built images go to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Registry {
    /// The cluster's own registry at `host`: pods log in with their service account token and
    /// trust the cluster's service CA (OpenShift's internal registry).
    Cluster { host: String },
    /// A registry that takes anonymous pushes over plain HTTP, such as a kind cluster's.
    Insecure { host: String },
}

impl Registry {
    pub fn host(&self) -> &str {
        match self {
            Self::Cluster { host } | Self::Insecure { host } => host,
        }
    }
}

/// Whose user ids a Job's pod runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PodUsers {
    /// Its own user namespace (`hostUsers: false`): root in a container is no one on the node.
    Namespaced,
    /// The node's, for clusters whose nodes cannot give pods user namespaces.
    Host,
}

/// Where and how every Job of one server runs.
#[derive(Debug, Clone)]
pub struct Site {
    pub namespace: String,
    pub queue: Option<String>,
    pub node_arch: Option<String>,
    pub resources: Resources,
    pub storage: StorageDriver,
    pub users: PodUsers,
    /// The longest any Job of this server runs.
    pub deadline_secs: i64,
    /// How long Kubernetes keeps an ended Job.
    pub ttl_secs: i32,
    /// The shared cache, without a trailing `/`.
    pub cache: String,
    /// The repository builds push to.
    pub image_repo: String,
    pub registry: Registry,
}

/// The blob store prefix a build's logs and results are archived under.
pub fn archive_prefix(sandbox: &str, build: &str) -> String {
    format!("builds/{sandbox}/{build}")
}

/// Where a context lives in the shared cache, by the hex of its digest.
pub fn context_key(digest: &str) -> String {
    format!("ctx/{}", digest.trim_start_matches("sha256:"))
}

/// One Job's identity: its name, the build it belongs to, and the labels it and its pod carry.
pub struct Ident<'a> {
    pub name: &'a str,
    pub build: &'a str,
    pub sandbox: &'a str,
    pub labels: &'a BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
}

/// A build: the context in the shared cache, the runner's arguments, and the tag it pushes.
pub struct BuildJob<'a> {
    pub context_digest: &'a str,
    /// The declared caches' directories, made before the runner restores into them.
    pub cache_dirs: &'a [String],
    /// `step::RUNNER`'s arguments after its log path: `<secs> <term> <steps> ...`.
    pub runner_args: &'a [String],
    pub local_tag: &'a str,
    pub limit_secs: i64,
}

/// A run of a succeeded build's image.
pub struct RunJob<'a> {
    pub n: u32,
    pub image: &'a str,
    pub cmd: &'a [String],
    pub fetch_paths: &'a [String],
    pub limit_secs: i64,
}

pub struct PushJob<'a> {
    pub source: &'a str,
    pub dest: &'a str,
}

const FETCH_BUILD: &str = r#"set -o pipefail
wget -qO- "$CACHE/$CTX" | tar -xzf - -C /workspace"#;

const FETCH_RUN: &str = r#"set -o pipefail
cp /busybox/busybox /buildit-tools/
mkdir -p /buildit-out/facts
if wget -qO /tmp/facts.tar "$CACHE/$PREFIX/facts.tar"; then tar -xf /tmp/facts.tar -C /buildit-out/facts; fi"#;

/// The archive sidecar: waits for SIGTERM, then uploads what the pod left in /buildit-out, each
/// upload bounded so all of them fit the grace period. An upload that fails is noted in the
/// error file, which goes up after the others; the exit record goes up last. Arguments: the exit
/// record's file, the error file, then the other files.
const ARCHIVE: &str = r#"exitf=$1; errf=$2; shift 2
up() {
  [ -e "/buildit-out/$1" ] || return 0
  curl -fsS --retry 2 --max-time 120 -o /dev/null -H @/buildit-archive/archive.auth \
    -T "/buildit-out/$1" "$CACHE/$PREFIX/$1" \
    || { echo "uploading $1 to the shared cache failed" | tee -a "/buildit-out/$errf" >&2; return 1; }
}
finish() {
  if [ -d /buildit-out/facts ] && [ -n "$(ls -A /buildit-out/facts)" ]; then tar -cf /buildit-out/facts.tar -C /buildit-out/facts .; fi
  for f in "$@"; do up "$f"; done
  up "$errf"
  up "$exitf"
  exit 0
}
trap 'finish "$@"' TERM
while :; do sleep 3600 & wait $!; done"#;

/// The build: storage setup, `step::RUNNER` with its log mirrored to stdout, then on success the
/// push, and the exit record and digest as the termination message. Arguments: the runner script,
/// the local tag, the destination, then the runner's arguments.
const BUILD: &str = r#"trap 'kill -TERM -1 2> /dev/null; exit 143' TERM
runner=$1; tag=$2; dest=$3; shift 3
log=/tmp/build.log
mkdir -p /tmp/containers-run /buildit-out/facts $CACHE_DIRS
printf '[storage]\ndriver = "%s"\ngraphroot = "%s/storage"\nrunroot = "/tmp/containers-run"\n' \
  "$STORAGE_DRIVER" "$STORAGE" > /tmp/storage.conf
for f in ccache.conf cache.json; do [ -e "/buildit-inputs/$f" ] && cp "/buildit-inputs/$f" /buildit-out/facts/; done
: > "$log"
sh -c "$runner" sh "$log" "$@" &
r=$!
tail -n +1 -f -s 0.2 --pid=$r "$log" &
wait $r
wait
rec=$(head -n 1 "$log.exit" 2> /dev/null)
[ -n "$rec" ] || rec=2
if [ "$rec" = 0 ]; then
  auth=/tmp/push-auth.json; echo '{}' > "$auth"
  certs=
  if [ -e /buildit-cluster/token ]; then
    certs="--cert-dir /buildit-cluster"
    buildah login --authfile "$auth" $certs -u buildit --password-stdin "$REGISTRY" < /buildit-cluster/token 2>&1
  else
    certs="--tls-verify=false"
  fi
  printf '$ buildah push %s\n' "$dest"
  if ! buildah push --authfile "$auth" $certs --digestfile /buildit-out/image "$tag" "docker://$dest" 2>&1; then
    echo "push to $dest failed"
    rec=1
    rm -f /buildit-out/image
  fi
fi
printf '%s\n' "$rec" > /buildit-out/build.exit
{ printf '%s\n' "$rec"; cat /buildit-out/image 2> /dev/null; } > /dev/termination-log"#;

/// A run: the command under a timeout with its output mirrored to the log, then the fetch_paths
/// that exist as a tar, and the exit record as the termination message. Arguments: the timeout,
/// the run number, the number of paths, the paths, then the command.
const RUN: &str = r#"B=/buildit-tools/busybox; secs=$1; n=$2; np=$3; shift 3
trap '$B kill -TERM -1 2> /dev/null; exit 143' TERM
: > /tmp/buildit-paths
while [ "$np" -gt 0 ]; do
  if [ -e "/$1" ]; then printf '%s\n' "$1" >> /tmp/buildit-paths; fi
  shift; np=$((np - 1))
done
log=/buildit-out/run-$n.log
s=$($B date +%s)
(set -o pipefail; $B timeout -s KILL "$secs" "$@" 2>&1 | $B tee "$log") &
wait $!
c=$?
t=
if [ "$c" -ne 0 ] && [ $(($($B date +%s) - s)) -ge "$secs" ]; then c=124; t=" timeout"; fi
if [ -s /tmp/buildit-paths ]; then
  (cd / && $B tar -cf "/buildit-out/run-$n.tar" -T /tmp/buildit-paths) 2> "/buildit-out/run-$n.fetch-err" \
    || $B rm -f "/buildit-out/run-$n.tar"
fi
printf '%s%s\n' "$c" "$t" > "/buildit-out/run-$n.exit"
$B cat "/buildit-out/run-$n.exit" > /dev/termination-log"#;

const PUSH: &str = r#"trap 'kill -TERM -1 2> /dev/null; exit 143' TERM
set -e
mkdir -p /tmp/d && cp /buildit-inputs/push.json /tmp/d/config.json
export DOCKER_CONFIG=/tmp/d
if [ -e /buildit-cluster/token ]; then
  crane auth login -u buildit --password-stdin "$REGISTRY" < /buildit-cluster/token
  export SSL_CERT_DIR=/buildit-cluster
  insecure=
else
  insecure=--insecure
fi
crane copy $insecure "$SRC" "$DEST" &
wait $!
crane digest $insecure "$DEST" > /dev/termination-log"#;

pub fn build_job(site: &Site, id: &Ident<'_>, b: &BuildJob<'_>) -> Job {
    let dest = format!("{}:{}-{}", site.image_repo, id.sandbox, id.build);
    let mut command = sh(BUILD);
    command.extend([
        crate::step::RUNNER.to_string(),
        b.local_tag.to_string(),
        dest,
    ]);
    command.extend(b.runner_args.iter().cloned());
    let mut vars = vec![
        env("BUILDAH_ISOLATION", "chroot"),
        env("CONTAINERS_STORAGE_CONF", "/tmp/storage.conf"),
        env("HOME", "/home/build"),
        env("STORAGE_DRIVER", site.storage.as_str()),
        env("STORAGE", STORAGE),
        env("REGISTRY", site.registry.host()),
        env("CACHE_DIRS", &b.cache_dirs.join(" ")),
    ];
    vars.sort_by(|a, b| a.name.cmp(&b.name));
    let mut mounts = vec![
        mount("workspace", WORKSPACE, false),
        mount("out", OUT, false),
        mount("storage", STORAGE, false),
        mount("inputs", INPUTS, true),
    ];
    mounts.extend(cluster_mount(site));
    let builder = core::Container {
        name: BUILDER.to_string(),
        image: Some(BUILDAH_IMAGE.to_string()),
        command: Some(command),
        env: Some(vars),
        security_context: Some(as_user(1000)),
        resources: site.resources.requirements(),
        termination_message_policy: Some("FallbackToLogsOnError".to_string()),
        volume_mounts: Some(mounts),
        ..Default::default()
    };
    let fetch = core::Container {
        name: "fetch".to_string(),
        image: Some(TOOLS_IMAGE.to_string()),
        command: Some(busybox_sh(FETCH_BUILD)),
        env: Some(vec![
            env("CACHE", &site.cache),
            env("CTX", &context_key(b.context_digest)),
        ]),
        volume_mounts: Some(vec![mount("workspace", WORKSPACE, false)]),
        ..Default::default()
    };
    let archive = archive(
        site,
        id,
        "build.exit",
        "build.archive-err",
        &["facts.tar", "image"],
    );
    let mut volumes = vec![
        empty_dir("workspace"),
        limited_dir("out", OUT_LIMIT),
        empty_dir("storage"),
        inputs_volume(
            id.name,
            &["snapshots", "snapshots.auth", "ccache.conf", "cache.json"],
        ),
        archive_volume(id.name),
    ];
    volumes.extend(cluster_volume(site));
    job(
        site,
        id,
        Kind::Build,
        b.limit_secs,
        vec![fetch, archive],
        builder,
        volumes,
    )
}

pub fn run_job(site: &Site, id: &Ident<'_>, r: &RunJob<'_>) -> Job {
    let mut command = vec![
        format!("{TOOLS}/busybox"),
        "sh".to_string(),
        "-c".to_string(),
    ];
    command.extend([
        RUN.to_string(),
        "sh".to_string(),
        r.limit_secs.to_string(),
        r.n.to_string(),
        r.fetch_paths.len().to_string(),
    ]);
    command.extend(r.fetch_paths.iter().cloned());
    command.extend(r.cmd.iter().cloned());
    let main = core::Container {
        name: RUN_CONTAINER.to_string(),
        image: Some(r.image.to_string()),
        command: Some(command),
        security_context: Some(as_user(0)),
        resources: site.resources.requirements(),
        termination_message_policy: Some("FallbackToLogsOnError".to_string()),
        volume_mounts: Some(vec![
            mount("out", OUT, false),
            mount("facts", crate::mcp::FACTS_MOUNT, true),
            mount("tools", TOOLS, true),
        ]),
        ..Default::default()
    };
    let prefix = archive_prefix(id.sandbox, id.build);
    let fetch = core::Container {
        name: "fetch".to_string(),
        image: Some(TOOLS_IMAGE.to_string()),
        command: Some(busybox_sh(FETCH_RUN)),
        env: Some(vec![env("CACHE", &site.cache), env("PREFIX", &prefix)]),
        volume_mounts: Some(vec![
            mount("facts", FACTS, false),
            mount("tools", TOOLS, false),
        ]),
        ..Default::default()
    };
    let n = r.n;
    let archive = archive(
        site,
        id,
        &format!("run-{n}.exit"),
        &format!("run-{n}.fetch-err"),
        &[&format!("run-{n}.tar")],
    );
    let volumes = vec![
        limited_dir("out", OUT_LIMIT),
        limited_dir("facts", FACTS_LIMIT),
        limited_dir("tools", TOOLS_LIMIT),
        archive_volume(id.name),
    ];
    let mut job = job(
        site,
        id,
        Kind::Run,
        r.limit_secs,
        vec![fetch, archive],
        main,
        volumes,
    );
    if let Some(labels) = job.metadata.labels.as_mut() {
        labels.insert(LABEL_RUN.to_string(), n.to_string());
    }
    job
}

pub fn push_job(site: &Site, id: &Ident<'_>, p: &PushJob<'_>) -> Job {
    let mut mounts = vec![mount("inputs", INPUTS, true)];
    mounts.extend(cluster_mount(site));
    let main = core::Container {
        name: BUILDER.to_string(),
        image: Some(TOOLS_IMAGE.to_string()),
        command: Some(busybox_sh(PUSH)),
        env: Some(vec![
            env("DEST", p.dest),
            env("REGISTRY", site.registry.host()),
            env("SRC", p.source),
        ]),
        security_context: Some(as_user(1000)),
        termination_message_policy: Some("FallbackToLogsOnError".to_string()),
        volume_mounts: Some(mounts),
        ..Default::default()
    };
    let mut volumes = vec![inputs_volume(id.name, &["push.json"])];
    volumes.extend(cluster_volume(site));
    job(site, id, Kind::Push, 1800, Vec::new(), main, volumes)
}

/// The Secret a Job's pod reads its inputs from, owned by the Job so it goes with it. The write
/// token's header is under `archive.auth`, which only the archive container mounts.
pub fn inputs_secret(
    namespace: &str,
    job: &str,
    job_uid: &str,
    labels: &BTreeMap<String, String>,
    files: BTreeMap<String, Vec<u8>>,
) -> core::Secret {
    core::Secret {
        metadata: ObjectMeta {
            name: Some(inputs_name(job)),
            namespace: Some(namespace.to_string()),
            labels: Some(labels.clone()),
            owner_references: Some(vec![OwnerReference {
                api_version: "batch/v1".to_string(),
                kind: "Job".to_string(),
                name: job.to_string(),
                uid: job_uid.to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        },
        type_: Some("Opaque".to_string()),
        data: Some(files.into_iter().map(|(k, v)| (k, ByteString(v))).collect()),
        ..Default::default()
    }
}

pub fn inputs_name(job: &str) -> String {
    format!("{job}-inputs")
}

/// Where a Job is, from it and its newest pod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observed {
    /// Suspended until Kueue admits it.
    Queued,
    Running,
    /// Ended, with the digest a build pushed.
    Ended {
        state: State,
        digest: Option<String>,
    },
}

pub fn observe(job: &Job, pod: Option<&core::Pod>, main: &str) -> Observed {
    let status = job.status.as_ref();
    let condition = |kind: &str| {
        status
            .and_then(|s| s.conditions.as_ref())
            .and_then(|cs| cs.iter().find(|c| c.type_ == kind && c.status == "True"))
    };
    let cancelled = job
        .metadata
        .annotations
        .as_ref()
        .is_some_and(|a| a.contains_key(ANNOTATION_CANCELLED));
    let terminated = pod
        .and_then(|p| p.status.as_ref())
        .and_then(|s| s.container_statuses.as_ref())
        .and_then(|cs| cs.iter().find(|c| c.name == main))
        .and_then(|c| c.state.as_ref())
        .and_then(|s| s.terminated.as_ref());
    let failed = condition("Failed").or_else(|| condition("FailureTarget"));
    let ended = condition("Complete").is_some() || failed.is_some();
    let recorded = terminated.and_then(|t| {
        let mut lines = t.message.as_deref().unwrap_or_default().lines();
        let state = lines.next().and_then(State::record)?;
        let digest = lines
            .next()
            .map(str::trim)
            .filter(|d| d.starts_with("sha256:"))
            .map(str::to_string);
        Some(Observed::Ended { state, digest })
    });
    if let Some(recorded) = recorded {
        return recorded;
    }
    if cancelled && (ended || terminated.is_some() || pod.is_none()) {
        return Observed::Ended {
            state: State::Cancelled,
            digest: None,
        };
    }
    if failed.is_some_and(|f| f.reason.as_deref() == Some("DeadlineExceeded")) {
        return Observed::Ended {
            state: State::Expired,
            digest: None,
        };
    }
    if let Some(t) = terminated {
        return Observed::Ended {
            state: State::Exited {
                code: t.exit_code,
                timed_out: false,
            },
            digest: None,
        };
    }
    if failed.is_some() {
        return Observed::Ended {
            state: State::Lost,
            digest: None,
        };
    }
    if job.spec.as_ref().and_then(|s| s.suspend) == Some(true) {
        return Observed::Queued;
    }
    Observed::Running
}

fn job(
    site: &Site,
    id: &Ident<'_>,
    kind: Kind,
    limit_secs: i64,
    init: Vec<core::Container>,
    main: core::Container,
    volumes: Vec<core::Volume>,
) -> Job {
    let mut labels = id.labels.clone();
    labels.insert(LABEL_KIND.to_string(), kind.as_str().to_string());
    let pod_labels = labels.clone();
    if let Some(queue) = &site.queue {
        labels.insert(LABEL_QUEUE.to_string(), queue.clone());
    }
    let deadline = (limit_secs + 2 * GRACE_SECS).min(site.deadline_secs);
    Job {
        metadata: ObjectMeta {
            name: Some(id.name.to_string()),
            namespace: Some(site.namespace.clone()),
            labels: Some(labels),
            annotations: Some(id.annotations.clone()),
            ..Default::default()
        },
        spec: Some(JobSpec {
            backoff_limit: Some(0),
            suspend: site.queue.as_ref().map(|_| true),
            ttl_seconds_after_finished: Some(site.ttl_secs),
            active_deadline_seconds: Some(deadline),
            template: core::PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(pod_labels),
                    ..Default::default()
                }),
                spec: Some(core::PodSpec {
                    restart_policy: Some("Never".to_string()),
                    host_users: match site.users {
                        PodUsers::Namespaced => Some(false),
                        PodUsers::Host => None,
                    },
                    automount_service_account_token: Some(false),
                    enable_service_links: Some(false),
                    termination_grace_period_seconds: Some(GRACE_SECS),
                    security_context: Some(core::PodSecurityContext {
                        fs_group: Some(1000),
                        ..Default::default()
                    }),
                    node_selector: site.node_arch.as_ref().map(|arch| {
                        BTreeMap::from([("kubernetes.io/arch".to_string(), arch.clone())])
                    }),
                    init_containers: (!init.is_empty()).then_some(init),
                    containers: vec![main],
                    volumes: Some(volumes),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn archive(
    site: &Site,
    id: &Ident<'_>,
    exit: &str,
    errors: &str,
    files: &[&str],
) -> core::Container {
    let mut command = vec!["sh".to_string(), "-c".to_string(), ARCHIVE.to_string()];
    command.push("sh".to_string());
    command.push(exit.to_string());
    command.push(errors.to_string());
    command.extend(files.iter().map(|f| f.to_string()));
    core::Container {
        name: "archive".to_string(),
        image: Some(BUILDAH_IMAGE.to_string()),
        restart_policy: Some("Always".to_string()),
        command: Some(command),
        env: Some(vec![
            env("CACHE", &site.cache),
            env("PREFIX", &archive_prefix(id.sandbox, id.build)),
        ]),
        security_context: Some(as_user(1000)),
        volume_mounts: Some(vec![
            mount("out", OUT, false),
            mount("archive-auth", ARCHIVE_AUTH, true),
        ]),
        ..Default::default()
    }
}

fn inputs_volume(job: &str, keys: &[&str]) -> core::Volume {
    core::Volume {
        name: "inputs".to_string(),
        secret: Some(core::SecretVolumeSource {
            secret_name: Some(inputs_name(job)),
            items: Some(keys.iter().map(|k| key_to_path(k)).collect()),
            optional: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn archive_volume(job: &str) -> core::Volume {
    core::Volume {
        name: "archive-auth".to_string(),
        secret: Some(core::SecretVolumeSource {
            secret_name: Some(inputs_name(job)),
            items: Some(vec![key_to_path("archive.auth")]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn key_to_path(key: &str) -> core::KeyToPath {
    core::KeyToPath {
        key: key.to_string(),
        path: key.to_string(),
        ..Default::default()
    }
}

/// The service account token and service CA a pod uses with the cluster's registry.
fn cluster_volume(site: &Site) -> Option<core::Volume> {
    let Registry::Cluster { .. } = site.registry else {
        return None;
    };
    Some(core::Volume {
        name: "cluster".to_string(),
        projected: Some(core::ProjectedVolumeSource {
            sources: Some(vec![
                core::VolumeProjection {
                    service_account_token: Some(core::ServiceAccountTokenProjection {
                        path: "token".to_string(),
                        expiration_seconds: Some(7200),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                core::VolumeProjection {
                    config_map: Some(core::ConfigMapProjection {
                        name: "openshift-service-ca.crt".to_string(),
                        items: Some(vec![key_to_path("service-ca.crt")]),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        }),
        ..Default::default()
    })
}

fn cluster_mount(site: &Site) -> Option<core::VolumeMount> {
    matches!(site.registry, Registry::Cluster { .. }).then(|| mount("cluster", CLUSTER, true))
}

fn as_user(uid: i64) -> core::SecurityContext {
    core::SecurityContext {
        run_as_user: Some(uid),
        run_as_group: Some(uid),
        ..Default::default()
    }
}

fn sh(script: &str) -> Vec<String> {
    ["sh", "-c", script, "sh"].map(String::from).to_vec()
}

fn busybox_sh(script: &str) -> Vec<String> {
    ["/busybox/sh", "-c", script].map(String::from).to_vec()
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

fn limited_dir(name: &str, limit: &str) -> core::Volume {
    core::Volume {
        name: name.to_string(),
        empty_dir: Some(core::EmptyDirVolumeSource {
            size_limit: Some(Quantity(limit.to_string())),
            ..Default::default()
        }),
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use k8s_openapi::api::batch::v1::Job;
    use k8s_openapi::api::core::v1::Pod;

    use crate::backend::{Resources, StorageDriver};
    use crate::jobs::{
        BuildJob, Ident, Observed, PodUsers, PushJob, Registry, RunJob, Site, build_job, observe,
        push_job, run_job,
    };
    use crate::step::State;

    fn site(queue: Option<&str>, registry: Registry) -> Site {
        Site {
            namespace: "builds".to_string(),
            queue: queue.map(str::to_string),
            node_arch: Some("amd64".to_string()),
            resources: Resources {
                requests: vec![("cpu".to_string(), "8".to_string())],
                limits: Vec::new(),
            },
            storage: StorageDriver::Overlay,
            users: PodUsers::Namespaced,
            deadline_secs: 3600,
            ttl_secs: 900,
            cache: "http://cache.builds.svc:8850".to_string(),
            image_repo: "registry.builds.svc:5000/builds/buildit".to_string(),
            registry,
        }
    }

    fn cluster() -> Registry {
        Registry::Cluster {
            host: "image-registry.openshift-image-registry.svc:5000".to_string(),
        }
    }

    fn ident<'a>(name: &'a str, labels: &'a BTreeMap<String, String>) -> Ident<'a> {
        Ident {
            name,
            build: "buildit-00000000000000a1",
            sandbox: "sb-a",
            labels,
            annotations: BTreeMap::from([("a".to_string(), "b".to_string())]),
        }
    }

    fn labels() -> BTreeMap<String, String> {
        BTreeMap::from([("buildit.dev/managed-by".to_string(), "mcp".to_string())])
    }

    #[test]
    fn a_build_is_a_queued_one_shot_job_that_pushes_and_archives() {
        let labels = labels();
        let args = ["1800".to_string(), "/tmp/t".to_string()];
        let dirs = ["/c/deps".to_string(), "/c/ccache".to_string()];
        let job = serde_json::to_value(build_job(
            &site(Some("builds"), cluster()),
            &ident("buildit-00000000000000a1", &labels),
            &BuildJob {
                context_digest: "sha256:ab",
                cache_dirs: &dirs,
                runner_args: &args,
                local_tag: "localhost/buildit/x:latest",
                limit_secs: 7200,
            },
        ))
        .unwrap();
        assert_eq!(
            job["metadata"]["labels"]["kueue.x-k8s.io/queue-name"],
            "builds"
        );
        assert_eq!(job["metadata"]["labels"]["buildit.dev/kind"], "build");
        assert_eq!(job["metadata"]["annotations"]["a"], "b");
        let spec = &job["spec"];
        assert_eq!(spec["suspend"], true);
        assert_eq!(spec["backoffLimit"], 0);
        assert_eq!(spec["ttlSecondsAfterFinished"], 900);
        assert_eq!(
            spec["activeDeadlineSeconds"], 3600,
            "capped by the site's deadline"
        );
        let pod = &spec["template"]["spec"];
        assert_eq!(pod["hostUsers"], false);
        assert_eq!(pod["automountServiceAccountToken"], false);
        assert_eq!(pod["terminationGracePeriodSeconds"], 600);
        assert_eq!(pod["nodeSelector"]["kubernetes.io/arch"], "amd64");
        assert_eq!(
            spec["template"]["metadata"]["labels"]["buildit.dev/kind"], "build",
            "the pod carries the Job's labels"
        );
        assert!(
            spec["template"]["metadata"]["labels"]
                .get("kueue.x-k8s.io/queue-name")
                .is_none()
        );

        let fetch = &pod["initContainers"][0];
        assert_eq!(fetch["name"], "fetch");
        assert_eq!(fetch["env"][1]["value"], "ctx/ab");
        let archive = &pod["initContainers"][1];
        assert_eq!(archive["name"], "archive");
        assert_eq!(archive["restartPolicy"], "Always", "a native sidecar");
        assert_eq!(
            archive["env"][1]["value"],
            "builds/sb-a/buildit-00000000000000a1"
        );
        let tail: Vec<&str> = archive["command"].as_array().unwrap()[4..]
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            tail,
            ["build.exit", "build.archive-err", "facts.tar", "image"]
        );
        assert_eq!(
            archive["volumeMounts"][1]["mountPath"], "/buildit-archive",
            "the write token is the archive's alone"
        );

        let builder = &pod["containers"][0];
        assert_eq!(builder["name"], "builder");
        assert_eq!(builder["securityContext"]["runAsUser"], 1000);
        assert_eq!(builder["resources"]["requests"]["cpu"], "8");
        let command: Vec<&str> = builder["command"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(command[4], crate::step::RUNNER);
        assert_eq!(command[5], "localhost/buildit/x:latest");
        assert_eq!(
            command[6],
            "registry.builds.svc:5000/builds/buildit:sb-a-buildit-00000000000000a1"
        );
        assert_eq!(&command[7..], ["1800", "/tmp/t"]);
        let env = |name: &str| {
            builder["env"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["name"] == name)
                .map(|e| e["value"].as_str().unwrap().to_string())
        };
        assert_eq!(env("CACHE_DIRS").as_deref(), Some("/c/deps /c/ccache"));
        assert_eq!(env("STORAGE_DRIVER").as_deref(), Some("overlay"));
        assert!(
            !builder["volumeMounts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["mountPath"] == "/buildit-archive"),
            "{builder}"
        );
        let volumes = pod["volumes"].as_array().unwrap();
        let inputs = volumes.iter().find(|v| v["name"] == "inputs").unwrap();
        assert_eq!(
            inputs["secret"]["secretName"],
            "buildit-00000000000000a1-inputs"
        );
        assert_eq!(
            inputs["secret"]["optional"], true,
            "a read-only build has no save header"
        );
        let cluster = volumes.iter().find(|v| v["name"] == "cluster").unwrap();
        assert_eq!(
            cluster["projected"]["sources"][1]["configMap"]["name"],
            "openshift-service-ca.crt"
        );

        let plain = serde_json::to_value(build_job(
            &site(
                None,
                Registry::Insecure {
                    host: "r:5000".to_string(),
                },
            ),
            &ident("buildit-00000000000000a1", &labels),
            &BuildJob {
                context_digest: "sha256:ab",
                cache_dirs: &[],
                runner_args: &args,
                local_tag: "t",
                limit_secs: 60,
            },
        ))
        .unwrap();
        assert!(plain["spec"].get("suspend").is_none());
        assert_eq!(plain["spec"]["activeDeadlineSeconds"], 1260);
        assert!(
            !plain["spec"]["template"]["spec"]["volumes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["name"] == "cluster")
        );
    }

    #[test]
    fn a_run_runs_its_command_in_the_built_image_and_archives_its_fetch() {
        let labels = labels();
        let cmd = ["sh".to_string(), "-c".to_string(), "make test".to_string()];
        let paths = ["out/a.txt".to_string()];
        let job = serde_json::to_value(run_job(
            &site(None, cluster()),
            &ident("buildit-00000000000000a1-run-3", &labels),
            &RunJob {
                n: 3,
                image: "r/b@sha256:ab",
                cmd: &cmd,
                fetch_paths: &paths,
                limit_secs: 600,
            },
        ))
        .unwrap();
        assert_eq!(job["metadata"]["labels"]["buildit.dev/run"], "3");
        assert_eq!(job["metadata"]["labels"]["buildit.dev/kind"], "run");
        let pod = &job["spec"]["template"]["spec"];
        let run = &pod["containers"][0];
        assert_eq!(run["image"], "r/b@sha256:ab");
        assert_eq!(
            run["securityContext"]["runAsUser"], 0,
            "root in the pod's user namespace"
        );
        let command: Vec<&str> = run["command"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(&command[..3], ["/buildit-tools/busybox", "sh", "-c"]);
        assert_eq!(
            &command[4..],
            ["sh", "600", "3", "1", "out/a.txt", "sh", "-c", "make test"]
        );
        assert_eq!(run["volumeMounts"][1]["mountPath"], "/buildit");
        assert_eq!(run["volumeMounts"][1]["readOnly"], true);
        let archive = &pod["initContainers"][1];
        let tail: Vec<&str> = archive["command"].as_array().unwrap()[4..]
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(tail, ["run-3.exit", "run-3.fetch-err", "run-3.tar"]);
        assert_eq!(pod["volumes"][0]["emptyDir"]["sizeLimit"], "2Gi");
        assert!(
            !pod["volumes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["name"] == "cluster"),
            "a run gets no registry token"
        );
    }

    #[test]
    fn a_push_copies_by_registry_without_a_pod_of_the_build() {
        let labels = labels();
        let job = serde_json::to_value(push_job(
            &site(None, cluster()),
            &ident("buildit-00000000000000a1-push-1", &labels),
            &PushJob {
                source: "r/b@sha256:ab",
                dest: "quay.io/acme/b:sb-a-x",
            },
        ))
        .unwrap();
        let pod = &job["spec"]["template"]["spec"];
        assert!(pod.get("initContainers").is_none());
        let env: BTreeMap<String, String> = pod["containers"][0]["env"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["name"].as_str().unwrap().to_string(),
                    e["value"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(env["SRC"], "r/b@sha256:ab");
        assert_eq!(env["DEST"], "quay.io/acme/b:sb-a-x");
        assert_eq!(pod["volumes"][0]["secret"]["items"][0]["key"], "push.json");
    }

    fn job(spec: serde_json::Value, status: serde_json::Value, cancelled: bool) -> Job {
        let mut meta = serde_json::json!({ "name": "buildit-1" });
        if cancelled {
            meta["annotations"] = serde_json::json!({ "buildit.dev/cancelled": "true" });
        }
        serde_json::from_value(serde_json::json!({
            "metadata": meta,
            "spec": spec,
            "status": status,
        }))
        .unwrap()
    }

    fn pod(terminated: Option<serde_json::Value>) -> Pod {
        let state = match terminated {
            Some(t) => serde_json::json!({ "terminated": t }),
            None => serde_json::json!({ "running": {} }),
        };
        serde_json::from_value(serde_json::json!({
            "metadata": { "name": "buildit-1-x" },
            "status": { "containerStatuses": [{
                "name": "builder", "image": "b", "imageID": "", "ready": false,
                "restartCount": 0, "state": state
            }]}
        }))
        .unwrap()
    }

    #[test]
    fn a_job_and_its_pod_say_where_the_step_is() {
        let template = serde_json::json!({ "spec": { "containers": [] } });
        let spec = |suspend: bool| serde_json::json!({ "suspend": suspend, "template": template });
        let none = serde_json::json!({});
        assert_eq!(
            observe(&job(spec(true), none.clone(), false), None, "builder"),
            Observed::Queued
        );
        assert_eq!(
            observe(
                &job(spec(false), none.clone(), false),
                Some(&pod(None)),
                "builder"
            ),
            Observed::Running
        );
        let ended = |message: Option<&str>, code: i32| {
            let mut t = serde_json::json!({ "exitCode": code });
            if let Some(m) = message {
                t["message"] = m.into();
            }
            observe(
                &job(spec(false), none.clone(), false),
                Some(&pod(Some(t))),
                "builder",
            )
        };
        assert_eq!(
            ended(Some("0\nsha256:ab\n"), 0),
            Observed::Ended {
                state: State::Exited {
                    code: 0,
                    timed_out: false
                },
                digest: Some("sha256:ab".to_string())
            }
        );
        assert_eq!(
            ended(Some("124 timeout\n"), 0),
            Observed::Ended {
                state: State::Exited {
                    code: 124,
                    timed_out: true
                },
                digest: None
            }
        );
        assert_eq!(
            ended(None, 137),
            Observed::Ended {
                state: State::Exited {
                    code: 137,
                    timed_out: false
                },
                digest: None
            },
            "killed before it wrote a record, by OOM say"
        );
        let deadline = serde_json::json!({ "conditions": [
            { "type": "Failed", "status": "True", "reason": "DeadlineExceeded" }
        ]});
        assert_eq!(
            observe(&job(spec(false), deadline, false), None, "builder"),
            Observed::Ended {
                state: State::Expired,
                digest: None
            }
        );
        let failed = serde_json::json!({ "conditions": [
            { "type": "Failed", "status": "True", "reason": "BackoffLimitExceeded" }
        ]});
        assert_eq!(
            observe(&job(spec(false), failed.clone(), true), None, "builder"),
            Observed::Ended {
                state: State::Cancelled,
                digest: None
            }
        );
        assert_eq!(
            observe(
                &job(spec(false), none.clone(), true),
                Some(&pod(None)),
                "builder"
            ),
            Observed::Running,
            "a cancel takes effect when the pod ends"
        );
        let finished = serde_json::json!({ "exitCode": 0, "message": "0\nsha256:ab\n" });
        assert_eq!(
            observe(
                &job(spec(false), none.clone(), true),
                Some(&pod(Some(finished))),
                "builder"
            ),
            Observed::Ended {
                state: State::Exited {
                    code: 0,
                    timed_out: false
                },
                digest: Some("sha256:ab".to_string())
            },
            "a step that ended before the cancel keeps its record"
        );
        let terminating = serde_json::json!({ "conditions": [
            { "type": "FailureTarget", "status": "True", "reason": "DeadlineExceeded" }
        ]});
        let killed = serde_json::json!({ "exitCode": 143, "message": "STEP 2/2: RUN sleep 600" });
        assert_eq!(
            observe(
                &job(spec(false), terminating, false),
                Some(&pod(Some(killed))),
                "builder"
            ),
            Observed::Ended {
                state: State::Expired,
                digest: None
            },
            "the deadline, while its pod still terminates"
        );
    }
}
