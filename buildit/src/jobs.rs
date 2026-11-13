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
use rmcp::schemars;

use crate::backend::{Resources, StorageDriver};
use crate::step::State;

pub const BUILDER: &str = "builder";
pub const RUN_CONTAINER: &str = "run";
pub const LAYER_CONTAINER: &str = "layer";
/// `dockerfile` or `layer` on a build's Job: how it builds.
pub const LABEL_RECIPE: &str = "buildit.dev/recipe";
pub const LABEL_KIND: &str = "buildit.dev/kind";
pub const LABEL_RUN: &str = "buildit.dev/run";
pub const ANNOTATION_CANCELLED: &str = "buildit.dev/cancelled";
const LABEL_QUEUE: &str = "kueue.x-k8s.io/queue-name";
const OPENSHIFT_BUILDER: &str = "io.openshift.builder";
const CRIO_USERNS_MODE: &str = "io.kubernetes.cri-o.userns-mode";
const CRIO_USERNS_AUTO: &str = "auto:size=65536;map-to-root=true";

const BUILDAH_IMAGE: &str = "quay.io/buildah/stable@sha256:7bb110e1d8b761d08e87d004ea4086e295a52319f3ea70ecef65da6dff7ceef0";
const KANIKO_IMAGE: &str = "gcr.io/kaniko-project/executor@sha256:9e22d0ba2c78cffbd47e2269aa6d3dc43fc3e3ecc6b61262399279dec5e67d45";
/// Where a BuildKit build's pod copies `buildkitd`, `buildctl`, `buildkit-runc` and `rootlesskit`
/// from.
const BUILDKIT_IMAGE: &str = "docker.io/moby/buildkit:v0.35.0-rootless";
const TOOLS_IMAGE: &str = "gcr.io/go-containerregistry/crane:debug";
pub const WORKSPACE: &str = "/workspace";
pub const OUT: &str = "/buildit-out";
pub const FACTS: &str = "/buildit-out/facts";
const TOOLS: &str = "/buildit-tools";
const INPUTS: &str = "/buildit-inputs";
const LAYER_DIR: &str = "/buildit-layer";
pub const CACHES: &str = "/buildit-caches";
const ARCHIVE_AUTH: &str = "/buildit-archive";
const CLUSTER: &str = "/buildit-cluster";
const STORAGE: &str = "/home/build/.local/share/containers";
/// buildah's temporary directory, on the storage volume.
const BUILD_TMP: &str = "/home/build/.local/share/containers/tmp";
/// What a pod may leave for the archive: facts, a run's log and the fetched paths `buildit mcp`
/// takes up to `step::FETCH_LIMIT` of.
const OUT_LIMIT: &str = "2Gi";
const FACTS_LIMIT: &str = "256Mi";
const TOOLS_LIMIT: &str = "64Mi";
const BUILDKIT_TOOLS_LIMIT: &str = "256Mi";
const BUILDKIT_DIR: &str = "/tmp/buildkit";
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

    pub fn is_cluster(&self) -> bool {
        matches!(self, Self::Cluster { .. })
    }
}

/// What builds a Dockerfile in a build pod.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    clap::ValueEnum,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Builder {
    /// `buildah bud`, then a push.
    #[default]
    Buildah,
    /// Rootless BuildKit, the builder `docker build` uses: a `buildkitd` for the one build.
    Buildkit,
    /// Kaniko's userspace executor, for restricted clusters that forbid builder mount syscalls.
    Kaniko,
}

impl Builder {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Buildah => "buildah",
            Self::Buildkit => "buildkit",
            Self::Kaniko => "kaniko",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        [Self::Buildah, Self::Buildkit, Self::Kaniko]
            .into_iter()
            .find(|b| b.as_str() == raw)
    }
}

/// A directory or file of the build pod that every `RUN` step of a Dockerfile build sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunMount {
    pub source: String,
    pub target: String,
    pub read_only: bool,
}

impl RunMount {
    fn mode(&self) -> &'static str {
        if self.read_only { "ro" } else { "rw" }
    }

    /// buildah's `--volume` value.
    pub fn volume(&self) -> String {
        format!("{}:{}:{}", self.source, self.target, self.mode())
    }

    /// The bind mount as an OCI runtime config lists it.
    fn oci(&self) -> serde_json::Value {
        serde_json::json!({
            "destination": self.target,
            "type": "bind",
            "source": self.source,
            "options": ["rbind", self.mode()],
        })
    }
}

/// Whose user ids a Job's pod runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PodUsers {
    /// Its own user namespace (`hostUsers: false`): root in a container is no one on the node.
    Namespaced,
    /// The node's, for clusters whose nodes cannot give pods user namespaces.
    Host,
    /// A CRI-O user namespace selected by OpenShift's builder workload.
    #[value(name = "openshift-builder")]
    OpenShiftBuilder,
}

fn pod_annotations(users: PodUsers) -> Option<BTreeMap<String, String>> {
    match users {
        PodUsers::OpenShiftBuilder => Some(BTreeMap::from([
            (OPENSHIFT_BUILDER.to_string(), "true".to_string()),
            (CRIO_USERNS_MODE.to_string(), CRIO_USERNS_AUTO.to_string()),
        ])),
        PodUsers::Namespaced | PodUsers::Host => None,
    }
}

/// Where and how every Job of one server runs.
#[derive(Debug, Clone)]
pub struct Site {
    pub namespace: String,
    pub queue: Option<String>,
    pub node_arch: Option<String>,
    pub resources: Resources,
    pub builder: Builder,
    pub storage: StorageDriver,
    /// Extra `buildah bud` arguments for every Dockerfile build.
    pub bud_args: Vec<String>,
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

/// A build: the context in the shared cache, the runner's arguments, and how its image is built.
pub struct BuildJob<'a> {
    pub context_digest: &'a str,
    /// The declared caches' directories, made before the runner restores into them.
    pub cache_dirs: &'a [String],
    /// `step::RUNNER`'s arguments after its log path: `<secs> <term> <stamps> <steps> ...`.
    pub runner_args: &'a [String],
    pub engine: Engine<'a>,
    pub limit_secs: i64,
}

/// What a build's runner drives, and what that needs from the pod.
pub enum Engine<'a> {
    /// `buildah bud` to `local_tag`, which the pod then pushes.
    Buildah { local_tag: &'a str },
    /// `buildctl build`, which pushes, against a `buildkitd` whose `RUN` steps get `mounts`.
    Buildkit { mounts: &'a [RunMount] },
    /// Kaniko builds and pushes entirely in userspace; its pod mounts provide Dockerfile caches.
    Kaniko { mounts: &'a [RunMount] },
}

/// The reference a build of `sandbox` pushes its image to.
pub fn build_dest(site: &Site, sandbox: &str, build: &str) -> String {
    format!("{}:{sandbox}-{build}", site.image_repo)
}

/// A build that runs a script in an existing image and appends one directory to it as a layer.
pub struct LayerJob<'a> {
    pub context_digest: &'a str,
    /// The image, by digest, the script runs in and the result is built on.
    pub base: &'a str,
    pub script: &'a str,
    /// The absolute directory the context is placed at and the layer holds.
    pub path: &'a str,
    /// Environment for the script: the build's args.
    pub env: &'a BTreeMap<String, String>,
    /// Declared caches, by name and the path the script sees them at.
    pub caches: &'a [(String, String)],
    pub ccache: bool,
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

const FETCH_LAYER: &str = r#"set -o pipefail
cp /busybox/busybox /ko-app/crane /buildit-tools/
wget -qO- "$CACHE/$CTX" | tar -xzf - -C /workspace"#;

const FETCH_RUN: &str = r#"set -o pipefail
cp /busybox/busybox /buildit-tools/
mkdir -p /buildit-out/facts
if wget -qO /tmp/facts.tar "$CACHE/$PREFIX/facts.tar"; then tar -xf /tmp/facts.tar -C /buildit-out/facts; fi"#;

/// A layer build's trusted init: the build's facts inputs into its facts, and its cluster caches
/// restored from the shared cache.
const PREPARE: &str = r#"for f in ccache.conf cache.json; do
  if [ -e "/buildit-inputs/$f" ]; then cp "/buildit-inputs/$f" /buildit-out/facts/; fi
done
if [ -s /buildit-inputs/snapshots ]; then sh -c "$RESTORE" sh /buildit-inputs/snapshots "$CACHE"; fi"#;

/// A layer build: the script in the base image, from `path`, then `path` as a tar without the
/// caches mounted under it, and the exit record as the termination message. Arguments: the
/// timeout, then `path`.
const LAYER: &str = r#"B=/buildit-tools/busybox; secs=$1; dir=$2
trap '$B kill -TERM -1 2> /dev/null; exit 143' TERM
s=$($B date +%s)
(cd "$dir" && $B timeout -s KILL "$secs" sh -ec "$BUILDIT_SCRIPT") &
wait $!
c=$?
t=
if [ "$c" -ne 0 ] && [ $(($($B date +%s) - s)) -ge "$secs" ]; then c=124; t=" timeout"; fi
if [ "$c" -eq 0 ]; then
  printf '%s\n' "$BUILDIT_EXCLUDE" | $B sed '/^$/d' > /tmp/buildit-exclude
  printf '$ tar %s\n' "$dir"
  $B tar -cf /buildit-layer/layer.tar -X /tmp/buildit-exclude -C / "${dir#/}" || c=1
fi
printf '%s%s\n' "$c" "$t" > /buildit-out/build.exit
$B cat /buildit-out/build.exit > /dev/termination-log"#;

/// What the archive sidecar does first for a layer build that succeeded: `crane append` of its
/// layer onto the base, which pushes no base layer the registry has, then the build's cluster
/// caches saved. A failed push turns the build's exit record into a failure.
const LAYER_PUSH: &str = r#"[ "$(head -n 1 /buildit-out/build.exit 2> /dev/null)" = 0 ] || exit 0
export DOCKER_CONFIG=/tmp/buildit-docker
mkdir -p "$DOCKER_CONFIG" && echo '{}' > "$DOCKER_CONFIG/config.json"
insecure=
if [ -e /buildit-cluster/token ]; then
  /buildit-tools/crane auth login -u buildit --password-stdin "$REGISTRY" < /buildit-cluster/token
  export SSL_CERT_DIR=/buildit-cluster
else
  insecure=--insecure
fi
if ref=$(/buildit-tools/crane append $insecure $PLATFORM -b "$BASE" -f /buildit-layer/layer.tar -t "$DEST"); then
  printf '%s\n' "${ref#*@}" > /buildit-out/image
else
  echo "pushing the layer to $DEST failed"
  echo 1 > /buildit-out/build.exit
  exit 0
fi
if [ -s /buildit-inputs/snapshots ] && [ -s /buildit-inputs/snapshots.auth ]; then
  sh -c "$SAVE" sh /buildit-inputs/snapshots "$CACHE" /buildit-inputs/snapshots.auth
fi"#;

/// Kaniko's registry credentials. It builds and pushes in userspace.
const KANIKO_SETUP: &str = r#"mkdir -p /tmp /kaniko/.docker /buildit-out/facts $CACHE_DIRS
if [ -e /buildit-cluster/token ]; then
  auth=$(printf 'buildit:%s' "$(cat /buildit-cluster/token)" | base64 | tr -d '\n')
  printf '{"auths":{"%s":{"auth":"%s"}}}\n' "$REGISTRY" "$auth" > /kaniko/.docker/config.json
else
  printf '{"auths":{}}\n' > /kaniko/.docker/config.json
fi"#;

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
  if [ -n "$BEFORE" ]; then sh -c "$BEFORE" >> "/buildit-out/$errf" 2>&1; fi
  if [ -d /buildit-out/facts ] && [ -n "$(ls -A /buildit-out/facts)" ]; then tar -cf /buildit-out/facts.tar -C /buildit-out/facts .; fi
  for f in "$@"; do up "$f"; done
  up "$errf"
  up "$exitf"
  exit 0
}
trap 'finish "$@"' TERM
while :; do sleep 3600 & wait $!; done"#;

/// A build's main container: [`BUILD_OPEN`], the builder's setup, [`BUILD_RUN`], the builder's
/// publish step, then [`BUILD_RECORD`]. Arguments: the runner script, the local tag, the
/// destination, then the runner's arguments.
const BUILD_OPEN: &str = r#"trap 'kill -TERM -1 2> /dev/null; exit 143' TERM
runner=$1; tag=$2; dest=$3; shift 3
log=/tmp/build.log"#;

/// buildah's storage, and pull access to the registry built images go to.
const BUILDAH_SETUP: &str = r#"mkdir -p /tmp/containers-run /buildit-out/facts "$HOME" "$TMPDIR" $CACHE_DIRS
printf '[storage]\ndriver = "%s"\ngraphroot = "%s/storage"\nrunroot = "/tmp/containers-run"\n' \
  "$STORAGE_DRIVER" "$STORAGE" > /tmp/storage.conf
printf 'unqualified-search-registries = ["docker.io"]\n' > /tmp/registries.conf
export REGISTRY_AUTH_FILE=/tmp/pull-auth.json; echo '{}' > "$REGISTRY_AUTH_FILE"
if [ -e /buildit-cluster/token ]; then
  mkdir -p "$HOME/.config/containers/certs.d/$REGISTRY"
  cp /buildit-cluster/service-ca.crt "$HOME/.config/containers/certs.d/$REGISTRY/ca.crt"
  buildah login -u buildit --password-stdin "$REGISTRY" < /buildit-cluster/token 2>&1
else
  printf '[[registry]]\nlocation = "%s"\ninsecure = true\n' "$REGISTRY" >> /tmp/registries.conf
fi"#;

/// A rootless `buildkitd` for this build, from the binaries the `buildkit` init container left
/// in /buildit-tools: its state on the storage volume, [`RUNC`] as its runtime, and pull and push
/// access to the registry built images go to. A daemon that does not come up ends the build.
const BUILDKIT_SETUP: &str = r#"bk=/tmp/buildkit
mkdir -p "$bk" "$XDG_RUNTIME_DIR" /buildit-out/facts "$TMPDIR" $CACHE_DIRS
export PATH="/buildit-tools:$PATH" DOCKER_CONFIG="$bk"
printf '%s\n' "$RUNC" > "$bk/runc"; chmod +x "$bk/runc"
printf '%s' "$RUN_MOUNTS" > "$bk/mounts.json"
printf '%s\n' "$RUN_TARGETS" > "$bk/targets"
{
  printf 'root = "%s/buildkit"\n[grpc]\n  address = ["%s"]\n[otel]\n  socketPath = "%s/otel-grpc.sock"\n' \
    "$STORAGE" "$BUILDKIT_HOST" "$bk"
  printf '[worker.oci]\n  rootless = true\n  noProcessSandbox = true\n  binary = "%s/runc"\n  snapshotter = "%s"\n' \
    "$bk" "$SNAPSHOTTER"
  printf '[worker.containerd]\n  enabled = false\n[registry."%s"]\n' "$REGISTRY"
  if [ -e /buildit-cluster/token ]; then
    printf '  ca = ["/buildit-cluster/service-ca.crt"]\n'
  else
    printf '  http = true\n'
  fi
} > "$bk/buildkitd.toml"
if [ -e /buildit-cluster/token ]; then
  export SSL_CERT_DIR=/buildit-cluster
  printf '{"auths":{"%s":{"auth":"%s"}}}\n' "$REGISTRY" \
    "$(printf 'buildit:%s' "$(cat /buildit-cluster/token)" | base64 -w 0)" > "$bk/config.json"
else
  echo '{}' > "$bk/config.json"
fi
(rootlesskit buildkitd --config "$bk/buildkitd.toml" > "$bk/buildkitd.log" 2>&1 &)
n=300
until buildctl debug workers > /dev/null 2>&1; do
  n=$((n - 1))
  if [ "$n" -le 0 ]; then
    echo "buildkitd did not start"; cat "$bk/buildkitd.log"
    echo 1 > /buildit-out/build.exit; echo 1 > /dev/termination-log
    exit 0
  fi
  sleep 0.1
done"#;

/// `buildkitd`'s OCI runtime: `buildkit-runc`, after three edits to the config of the `RUN` step
/// it is about to start. The hostname goes, /dev/pts becomes a bind of the pod's, and the build's
/// [`RunMount`]s go first in the mounts. Once the step has ended, the mount points the runtime
/// made for them in the step's root, and the directories it made to hold those, go again where
/// the step left them empty.
const RUNC: &str = r#"#!/bin/bash
b=; prev=
for a in "$@"; do [ "$prev" = --bundle ] && b=$a; prev=$a; done
if [ -z "$b" ] || [ ! -f "$b/config.json" ]; then exec /buildit-tools/buildkit-runc "$@"; fi
sed -i -e 's/"hostname":"[^"]*",//' \
  -e 's#{"destination":"/dev/pts","type":"devpts","source":"devpts","options":\[[^]]*\]}#{"destination":"/dev/pts","type":"bind","source":"/dev/pts","options":["rbind","nosuid","noexec"]}#' \
  "$b/config.json"
c=$(< "$b/config.json"); m=$(< /tmp/buildkit/mounts.json)
printf '%s' "${c/'"mounts":['/'"mounts":['"$m"}" > "$b/config.json"
root=$b/rootfs; made=()
while IFS= read -r t; do
  [ -n "$t" ] || continue
  top=; p=$t
  while [ "$p" != / ] && [ ! -e "$root$p" ] && [ ! -L "$root$p" ]; do top=$p; p=$(dirname "$p"); done
  [ -n "$top" ] && made+=("$t" "$top")
done < /tmp/buildkit/targets
/buildit-tools/buildkit-runc "$@" &
r=$!
trap 'kill -TERM "$r" 2> /dev/null' TERM INT
wait "$r"; code=$?
while kill -0 "$r" 2> /dev/null; do wait "$r"; code=$?; done
for ((i = 0; i < ${#made[@]}; i += 2)); do
  p=${made[i]}; top=${made[i + 1]}
  while :; do
    if [ -d "$root$p" ] && [ ! -L "$root$p" ]; then rmdir "$root$p" 2> /dev/null || break
    elif [ -f "$root$p" ] && [ ! -s "$root$p" ]; then rm -f "$root$p"
    else break; fi
    [ "$p" = "$top" ] && break
    p=$(dirname "$p")
  done
done
exit "$code""#;

/// `step::RUNNER` with its log mirrored to stdout, and its exit record.
const BUILD_RUN: &str = r#"for f in ccache.conf cache.json; do [ -e "/buildit-inputs/$f" ] && cp "/buildit-inputs/$f" /buildit-out/facts/; done
: > "$log"
sh -c "$runner" sh "$log" "$@" &
r=$!
mirror() {
  seen=0
  while kill -0 "$r" 2> /dev/null; do
    lines=$(wc -l < "$log")
    if [ "$lines" -gt "$seen" ]; then sed -n "$((seen + 1)),${lines}p" "$log"; seen=$lines; fi
    sleep 1
  done
  lines=$(wc -l < "$log")
  [ "$lines" -le "$seen" ] || sed -n "$((seen + 1)),${lines}p" "$log"
}
mirror &
wait $r
wait
rec=$(head -n 1 "$log.exit" 2> /dev/null)
[ -n "$rec" ] || rec=2"#;

/// The push of what `bud` built.
const BUILDAH_PUBLISH: &str = r#"if [ "$rec" = 0 ]; then
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
fi"#;

/// The digest of the image `buildctl` pushed, from the metadata file it wrote.
const BUILDKIT_PUBLISH: &str = r#"if [ "$rec" = 0 ]; then
  sed -n 's/.*"containerimage.digest": *"\(sha256:[0-9a-f]*\)".*/\1/p' "$bk/metadata.json" 2> /dev/null \
    | head -n 1 > /buildit-out/image
  if [ ! -s /buildit-out/image ]; then
    echo "the build of $dest left no image digest"
    rec=1
    rm -f /buildit-out/image
  fi
fi"#;

/// The exit record, and with the digest the termination message.
const BUILD_RECORD: &str = r#"printf '%s\n' "$rec" > /buildit-out/build.exit
{ printf '%s\n' "$rec"; cat /buildit-out/image 2> /dev/null; } > /dev/termination-log"#;

/// Copies BuildKit's static binaries to where the builder container runs them.
const BUILDKIT_TOOLS: &str = "cp /usr/bin/buildkitd /usr/bin/buildctl /usr/bin/buildkit-runc /usr/bin/rootlesskit /buildit-tools/";

/// A run: the command under a timeout with its output mirrored to the log, then the fetch_paths
/// that exist as a tar, and the exit record as the termination message. Arguments: the timeout,
/// the run number, the number of paths, the paths, then the command.
const RUN: &str = r#"B=/buildit-tools/busybox; secs=$1; n=$2; np=$3; shift 3
trap '$B kill -TERM -1 2> /dev/null; exit 143' TERM
: > /tmp/buildit-want
while [ "$np" -gt 0 ]; do
  printf '%s\n' "$1" >> /tmp/buildit-want
  shift; np=$((np - 1))
done
log=/buildit-out/run-$n.log
s=$($B date +%s)
(set -o pipefail; $B timeout -s KILL "$secs" "$@" 2>&1 | $B tee "$log") &
wait $!
c=$?
t=
if [ "$c" -ne 0 ] && [ $(($($B date +%s) - s)) -ge "$secs" ]; then c=124; t=" timeout"; fi
: > /tmp/buildit-paths
while IFS= read -r p; do
  if [ -e "/$p" ]; then printf '%s\n' "$p" >> /tmp/buildit-paths; fi
done < /tmp/buildit-want
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
    let (setup, publish, local_tag) = match b.engine {
        Engine::Buildah { local_tag } => (BUILDAH_SETUP, BUILDAH_PUBLISH, local_tag),
        Engine::Buildkit { .. } => (BUILDKIT_SETUP, BUILDKIT_PUBLISH, ""),
        Engine::Kaniko { .. } => (KANIKO_SETUP, "", ""),
    };
    let script = [BUILD_OPEN, setup, BUILD_RUN, publish, BUILD_RECORD].join("\n");
    let mut command = sh(&script);
    command.extend([
        crate::step::RUNNER.to_string(),
        local_tag.to_string(),
        build_dest(site, id.sandbox, id.build),
    ]);
    command.extend(b.runner_args.iter().cloned());
    let kaniko = matches!(b.engine, Engine::Kaniko { .. });
    let mut vars = vec![
        env("HOME", if kaniko { "/tmp" } else { "/home/build" }),
        env("STORAGE", STORAGE),
        env("REGISTRY", site.registry.host()),
        env("CACHE_DIRS", &b.cache_dirs.join(" ")),
        env("TMPDIR", if kaniko { "/tmp" } else { BUILD_TMP }),
    ];
    let mut mounts = vec![
        mount("workspace", WORKSPACE, false),
        mount("out", OUT, false),
        mount("storage", STORAGE, false),
        mount("inputs", INPUTS, true),
    ];
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
    let mut init = vec![fetch];
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
    match b.engine {
        Engine::Buildah { .. } => vars.extend([
            env("BUILDAH_ISOLATION", "chroot"),
            env("CONTAINERS_REGISTRIES_CONF", "/tmp/registries.conf"),
            env("CONTAINERS_STORAGE_CONF", "/tmp/storage.conf"),
            env("STORAGE_DRIVER", site.storage.as_str()),
        ]),
        Engine::Buildkit { mounts: run_mounts } => {
            vars.extend([
                env("BUILDKIT_HOST", &buildkit_host()),
                env("RUNC", RUNC),
                env("RUN_MOUNTS", &run_mounts_json(run_mounts)),
                env(
                    "RUN_TARGETS",
                    &run_mounts
                        .iter()
                        .map(|m| m.target.as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                env("SNAPSHOTTER", snapshotter(site.storage)),
                env("XDG_RUNTIME_DIR", &format!("{BUILDKIT_DIR}/run")),
            ]);
            mounts.push(mount("tools", TOOLS, true));
            init.push(core::Container {
                name: "buildkit".to_string(),
                image: Some(BUILDKIT_IMAGE.to_string()),
                command: Some(["sh", "-c", BUILDKIT_TOOLS].map(String::from).to_vec()),
                security_context: Some(as_user(1000)),
                volume_mounts: Some(vec![mount("tools", TOOLS, false)]),
                ..Default::default()
            });
            volumes.push(limited_dir("tools", BUILDKIT_TOOLS_LIMIT));
        }
        Engine::Kaniko { mounts: run_mounts } => {
            mounts.push(mount("kaniko-tmp", "/tmp", false));
            mounts.push(mount("kaniko-docker", "/kaniko/.docker", false));
            volumes.push(empty_dir("kaniko-tmp"));
            volumes.push(empty_dir("kaniko-docker"));
            for (i, run_mount) in run_mounts.iter().enumerate() {
                if run_mount.source == FACTS {
                    mounts.push(mount("facts-run", &run_mount.source, false));
                    mounts.push(mount("facts-run", &run_mount.target, run_mount.read_only));
                    volumes.push(empty_dir("facts-run"));
                } else if run_mount.target == "/etc/ccache.conf" {
                    mounts.push(core::VolumeMount {
                        sub_path: Some("ccache.conf".to_string()),
                        ..mount("inputs", &run_mount.target, true)
                    });
                } else {
                    let name = format!("run-cache-{i}");
                    mounts.push(mount(&name, &run_mount.source, false));
                    mounts.push(mount(&name, &run_mount.target, run_mount.read_only));
                    volumes.push(empty_dir(&name));
                }
            }
        }
    }
    vars.sort_by(|a, b| a.name.cmp(&b.name));
    mounts.extend(cluster_mount(site));
    let builder = core::Container {
        name: BUILDER.to_string(),
        image: Some(match b.engine {
            Engine::Kaniko { .. } => KANIKO_IMAGE.to_string(),
            Engine::Buildah { .. } | Engine::Buildkit { .. } => BUILDAH_IMAGE.to_string(),
        }),
        command: Some(command),
        env: Some(vars),
        security_context: Some(as_user(if matches!(b.engine, Engine::Kaniko { .. }) {
            0
        } else {
            1000
        })),
        resources: site.resources.requirements(),
        termination_message_policy: Some("FallbackToLogsOnError".to_string()),
        volume_mounts: Some(mounts),
        ..Default::default()
    };
    let mut archive = archive(
        site,
        id,
        "build.exit",
        "build.archive-err",
        &["facts.tar", "image"],
    );
    if matches!(b.engine, Engine::Kaniko { .. }) {
        archive
            .volume_mounts
            .get_or_insert_default()
            .push(mount("facts-run", FACTS, false));
    }
    init.push(archive);
    volumes.extend(cluster_volume(site));
    job(site, id, Kind::Build, b.limit_secs, init, builder, volumes)
}

/// Where a build's `buildkitd` listens, which `buildctl` reads from `BUILDKIT_HOST`.
fn buildkit_host() -> String {
    format!("unix://{BUILDKIT_DIR}/buildkitd.sock")
}

/// The file `buildctl` leaves the pushed image's digest in.
pub fn buildkit_metadata() -> String {
    format!("{BUILDKIT_DIR}/metadata.json")
}

/// BuildKit's snapshotter for a storage driver: `native` copies each layer in full, as `vfs` does.
fn snapshotter(storage: StorageDriver) -> &'static str {
    match storage {
        StorageDriver::Vfs => "native",
        StorageDriver::Overlay => "overlayfs",
    }
}

/// `mounts` as [`RUNC`] splices them into a config's `"mounts":[`: each object followed by a
/// comma.
fn run_mounts_json(mounts: &[RunMount]) -> String {
    mounts.iter().map(|m| format!("{},", m.oci())).collect()
}

pub fn layer_job(site: &Site, id: &Ident<'_>, l: &LayerJob<'_>) -> Job {
    let dest = format!("{}:{}-{}", site.image_repo, id.sandbox, id.build);
    let mut vars: Vec<core::EnvVar> = l.env.iter().map(|(k, v)| env(k, v)).collect();
    vars.push(env("BUILDIT_SCRIPT", l.script));
    let exclude: Vec<String> = l
        .caches
        .iter()
        .filter_map(|(_, p)| p.strip_prefix(l.path.trim_end_matches('/')))
        .filter(|rest| rest.starts_with('/'))
        .map(|rest| {
            format!(
                "{}{rest}",
                l.path.trim_start_matches('/').trim_end_matches('/')
            )
        })
        .collect();
    vars.push(env("BUILDIT_EXCLUDE", &exclude.join("\n")));
    if l.ccache {
        vars.push(env(
            "CCACHE_CONFIGPATH",
            &format!("{}/ccache.conf", crate::mcp::FACTS_MOUNT),
        ));
    }
    let mut command = vec![
        format!("{TOOLS}/busybox"),
        "sh".to_string(),
        "-c".to_string(),
    ];
    command.extend([
        LAYER.to_string(),
        "sh".to_string(),
        l.limit_secs.to_string(),
        l.path.to_string(),
    ]);
    let mut mounts = vec![
        mount("workspace", l.path, false),
        mount("out", OUT, false),
        mount("facts", crate::mcp::FACTS_MOUNT, false),
        mount("layer", LAYER_DIR, false),
        mount("tools", TOOLS, true),
    ];
    for (name, path) in l.caches {
        mounts.push(core::VolumeMount {
            sub_path: Some(name.clone()),
            ..mount("caches", path, false)
        });
    }
    let main = core::Container {
        name: LAYER_CONTAINER.to_string(),
        image: Some(l.base.to_string()),
        command: Some(command),
        env: Some(vars),
        security_context: Some(as_user(0)),
        resources: site.resources.requirements(),
        termination_message_policy: Some("FallbackToLogsOnError".to_string()),
        volume_mounts: Some(mounts),
        ..Default::default()
    };
    let fetch = core::Container {
        name: "fetch".to_string(),
        image: Some(TOOLS_IMAGE.to_string()),
        command: Some(busybox_sh(FETCH_LAYER)),
        env: Some(vec![
            env("CACHE", &site.cache),
            env("CTX", &context_key(l.context_digest)),
        ]),
        volume_mounts: Some(vec![
            mount("workspace", WORKSPACE, false),
            mount("tools", TOOLS, false),
        ]),
        ..Default::default()
    };
    let prepare = core::Container {
        name: "prepare".to_string(),
        image: Some(BUILDAH_IMAGE.to_string()),
        command: Some(sh(PREPARE)),
        env: Some(vec![
            env("CACHE", &site.cache),
            env("RESTORE", crate::step::RESTORE),
        ]),
        security_context: Some(as_user(1000)),
        volume_mounts: Some(vec![
            mount("facts", FACTS, false),
            mount("caches", CACHES, false),
            mount("inputs", INPUTS, true),
        ]),
        ..Default::default()
    };
    let mut archive = archive(
        site,
        id,
        "build.exit",
        "build.archive-err",
        &["facts.tar", "image"],
    );
    let platform = site
        .node_arch
        .as_ref()
        .map(|arch| format!("--platform=linux/{arch}"))
        .unwrap_or_default();
    archive.env.get_or_insert_default().extend([
        env("BEFORE", LAYER_PUSH),
        env("BASE", l.base),
        env("DEST", &dest),
        env("PLATFORM", &platform),
        env("REGISTRY", site.registry.host()),
        env("SAVE", crate::step::SAVE),
    ]);
    let archive_mounts = archive.volume_mounts.get_or_insert_default();
    archive_mounts.extend([
        mount("facts", FACTS, false),
        mount("layer", LAYER_DIR, true),
        mount("tools", TOOLS, true),
        mount("caches", CACHES, false),
        mount("inputs", INPUTS, true),
    ]);
    archive_mounts.extend(cluster_mount(site));
    let mut volumes = vec![
        empty_dir("workspace"),
        limited_dir("out", OUT_LIMIT),
        limited_dir("facts", FACTS_LIMIT),
        empty_dir("layer"),
        limited_dir("tools", TOOLS_LIMIT),
        empty_dir("caches"),
        inputs_volume(
            id.name,
            &["snapshots", "snapshots.auth", "ccache.conf", "cache.json"],
        ),
        archive_volume(id.name),
    ];
    volumes.extend(cluster_volume(site));
    let mut job = job(
        site,
        id,
        Kind::Build,
        l.limit_secs,
        vec![fetch, prepare, archive],
        main,
        volumes,
    );
    if let Some(labels) = job.metadata.labels.as_mut() {
        labels.insert(LABEL_RECIPE.to_string(), "layer".to_string());
    }
    job
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
                    annotations: pod_annotations(site.users),
                    ..Default::default()
                }),
                spec: Some(core::PodSpec {
                    restart_policy: Some("Never".to_string()),
                    host_users: match site.users {
                        PodUsers::Namespaced => Some(false),
                        PodUsers::Host | PodUsers::OpenShiftBuilder => None,
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
        BuildJob, Builder, Engine, FACTS, Ident, KANIKO_IMAGE, LayerJob, Observed, PodUsers,
        PushJob, Registry, RunJob, RunMount, Site, build_job, layer_job, observe, push_job,
        run_job,
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
            builder: Builder::Buildah,
            storage: StorageDriver::Overlay,
            bud_args: Vec::new(),
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
                engine: Engine::Buildah {
                    local_tag: "localhost/buildit/x:latest",
                },
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
        assert_eq!(
            env("CONTAINERS_REGISTRIES_CONF").as_deref(),
            Some("/tmp/registries.conf")
        );
        assert!(
            command[2].contains(
                r#"printf 'unqualified-search-registries = ["docker.io"]\n' > /tmp/registries.conf"#
            ),
            "an unqualified FROM resolves to docker.io without a prompt"
        );
        assert!(
            command[2].contains(
                r#"cp /buildit-cluster/service-ca.crt "$HOME/.config/containers/certs.d/$REGISTRY/ca.crt""#
            ) && command[2].contains(
                r#"buildah login -u buildit --password-stdin "$REGISTRY" < /buildit-cluster/token"#
            ),
            "a FROM of an earlier build's image pulls from the cluster's registry"
        );
        assert!(
            command[2].contains(
                r#"printf '[[registry]]\nlocation = "%s"\ninsecure = true\n' "$REGISTRY" >> /tmp/registries.conf"#
            ),
            "and from a plain HTTP one"
        );
        assert_eq!(
            env("REGISTRY").as_deref(),
            Some("image-registry.openshift-image-registry.svc:5000")
        );
        assert_eq!(
            env("TMPDIR").as_deref(),
            Some("/home/build/.local/share/containers/tmp"),
            "on the storage volume, where an overlay upper directory mounts"
        );
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
                engine: Engine::Buildah { local_tag: "t" },
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
    fn an_openshift_builder_uses_the_crio_user_namespace() {
        let labels = labels();
        let mut site = site(None, cluster());
        site.users = PodUsers::OpenShiftBuilder;
        let args = ["1800".to_string(), "/tmp/t".to_string()];
        let job = serde_json::to_value(build_job(
            &site,
            &ident("buildit-00000000000000a1", &labels),
            &BuildJob {
                context_digest: "sha256:ab",
                cache_dirs: &[],
                runner_args: &args,
                engine: Engine::Buildah {
                    local_tag: "localhost/buildit/x:latest",
                },
                limit_secs: 600,
            },
        ))
        .unwrap();
        let template = &job["spec"]["template"];
        assert!(template["spec"].get("hostUsers").is_none());
        assert_eq!(
            template["metadata"]["annotations"]["io.openshift.builder"],
            "true"
        );
        assert_eq!(
            template["metadata"]["annotations"]["io.kubernetes.cri-o.userns-mode"],
            "auto:size=65536;map-to-root=true"
        );
    }

    fn buildkit_job(registry: Registry, storage: StorageDriver) -> serde_json::Value {
        let labels = labels();
        let args = ["1800".to_string(), "buildctl".to_string()];
        let dirs = ["/c/deps".to_string()];
        let mounts = [
            RunMount {
                source: "/c/deps".to_string(),
                target: "/root/a \"b\"".to_string(),
                read_only: true,
            },
            RunMount {
                source: "/buildit-out/facts".to_string(),
                target: "/buildit".to_string(),
                read_only: false,
            },
        ];
        let site = Site {
            builder: Builder::Buildkit,
            storage,
            ..site(Some("builds"), registry)
        };
        serde_json::to_value(build_job(
            &site,
            &ident("buildit-00000000000000a1", &labels),
            &BuildJob {
                context_digest: "sha256:ab",
                cache_dirs: &dirs,
                runner_args: &args,
                engine: Engine::Buildkit { mounts: &mounts },
                limit_secs: 600,
            },
        ))
        .unwrap()
    }

    #[test]
    fn a_kaniko_build_mounts_caches_and_facts_without_privilege() {
        let labels = labels();
        let site = site(None, cluster());
        let runner_args = ["1800".to_string(), "/kaniko/executor".to_string()];
        let mounts = [
            RunMount {
                source: "/home/build/.local/share/containers/buildit-caches/deps".to_string(),
                target: "/cache".to_string(),
                read_only: false,
            },
            RunMount {
                source: FACTS.to_string(),
                target: "/buildit".to_string(),
                read_only: false,
            },
            RunMount {
                source: format!("{FACTS}/ccache.conf"),
                target: "/etc/ccache.conf".to_string(),
                read_only: true,
            },
        ];
        let job = serde_json::to_value(build_job(
            &site,
            &ident("buildit-00000000000000a1", &labels),
            &BuildJob {
                context_digest: "sha256:ab",
                cache_dirs: &[mounts[0].source.clone()],
                runner_args: &runner_args,
                engine: Engine::Kaniko { mounts: &mounts },
                limit_secs: 600,
            },
        ))
        .unwrap();
        let pod = &job["spec"]["template"]["spec"];
        assert_eq!(pod["hostUsers"], false);
        let builder = &pod["containers"][0];
        assert_eq!(builder["image"], KANIKO_IMAGE);
        assert_eq!(builder["securityContext"]["runAsUser"], 0);
        assert!(builder["securityContext"].get("privileged").is_none());
        let script = builder["command"][2].as_str().unwrap();
        assert!(script.contains("mirror()"));
        assert!(!script.contains("tail -n +1 -f"));
        assert!(!script.contains("--pid"));
        let mounts = builder["volumeMounts"].as_array().unwrap();
        assert!(mounts.iter().any(|m| m["mountPath"] == "/cache"));
        assert!(mounts.iter().any(|m| m["mountPath"] == "/buildit"));
        assert!(mounts.iter().any(|m| m["mountPath"] == "/tmp"));
        assert!(mounts.iter().any(|m| m["mountPath"] == "/kaniko/.docker"));
        assert_eq!(
            builder["env"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["name"] == "HOME")
                .unwrap()["value"],
            "/tmp"
        );
        assert_eq!(
            builder["env"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["name"] == "TMPDIR")
                .unwrap()["value"],
            "/tmp"
        );
        assert!(
            mounts
                .iter()
                .any(|m| { m["mountPath"] == "/etc/ccache.conf" && m["subPath"] == "ccache.conf" })
        );
        let archive = &pod["initContainers"].as_array().unwrap().last().unwrap();
        assert!(
            archive["volumeMounts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["name"] == "facts-run" && m["mountPath"] == FACTS)
        );
    }

    #[test]
    fn a_buildkit_build_runs_buildkitd_from_copied_binaries_in_the_buildah_builder() {
        let job = buildkit_job(cluster(), StorageDriver::Overlay);
        let pod = &job["spec"]["template"]["spec"];
        assert_eq!(pod["hostUsers"], false);
        let init: Vec<(&str, &str)> = pod["initContainers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["name"].as_str().unwrap(), c["image"].as_str().unwrap()))
            .collect();
        assert_eq!(
            init,
            [
                ("fetch", "gcr.io/go-containerregistry/crane:debug"),
                ("buildkit", "docker.io/moby/buildkit:v0.35.0-rootless"),
                (
                    "archive",
                    "quay.io/buildah/stable@sha256:7bb110e1d8b761d08e87d004ea4086e295a52319f3ea70ecef65da6dff7ceef0",
                ),
            ]
        );
        let tools = &pod["initContainers"][1];
        assert_eq!(tools["securityContext"]["runAsUser"], 1000);
        assert!(tools.get("restartPolicy").is_none(), "{tools}");
        assert_eq!(
            tools["command"][2],
            "cp /usr/bin/buildkitd /usr/bin/buildctl /usr/bin/buildkit-runc /usr/bin/rootlesskit /buildit-tools/"
        );
        assert_eq!(
            tools["volumeMounts"],
            serde_json::json!([{ "name": "tools", "mountPath": "/buildit-tools" }])
        );
        let volumes = pod["volumes"].as_array().unwrap();
        let dir = volumes.iter().find(|v| v["name"] == "tools").unwrap();
        assert_eq!(dir["emptyDir"]["sizeLimit"], "256Mi");

        let builder = &pod["containers"][0];
        assert_eq!(builder["name"], "builder");
        assert_eq!(
            builder["image"],
            "quay.io/buildah/stable@sha256:7bb110e1d8b761d08e87d004ea4086e295a52319f3ea70ecef65da6dff7ceef0"
        );
        assert_eq!(builder["securityContext"]["runAsUser"], 1000);
        assert!(builder["securityContext"].get("privileged").is_none());
        assert!(builder["securityContext"].get("seccompProfile").is_none());
        let command: Vec<&str> = builder["command"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(command[4], crate::step::RUNNER);
        assert_eq!(command[5], "", "nothing is tagged in the pod");
        assert_eq!(
            command[6],
            "registry.builds.svc:5000/builds/buildit:sb-a-buildit-00000000000000a1"
        );
        assert_eq!(&command[7..], ["1800", "buildctl"]);
        let script = command[2];
        for part in [
            "runner=$1; tag=$2; dest=$3; shift 3",
            r#"(rootlesskit buildkitd --config "$bk/buildkitd.toml" > "$bk/buildkitd.log" 2>&1 &)"#,
            "export SSL_CERT_DIR=/buildit-cluster",
            r#"printf '[worker.oci]\n  rootless = true\n  noProcessSandbox = true\n  binary = "%s/runc"\n  snapshotter = "%s"\n'"#,
            r#"printf '  ca = ["/buildit-cluster/service-ca.crt"]\n'"#,
            r#"printf '  http = true\n'"#,
            r#""$(printf 'buildit:%s' "$(cat /buildit-cluster/token)" | base64 -w 0)" > "$bk/config.json""#,
            r#"sh -c "$runner" sh "$log" "$@" &"#,
            r#""$bk/metadata.json""#,
            r#"printf '%s\n' "$rec" > /buildit-out/build.exit"#,
        ] {
            assert!(script.contains(part), "{part}\nis not in\n{script}");
        }
        assert!(!script.contains("buildah"), "{script}");
        let env = |name: &str| {
            builder["env"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["name"] == name)
                .map(|e| e["value"].as_str().unwrap().to_string())
        };
        assert_eq!(
            env("BUILDKIT_HOST").as_deref(),
            Some("unix:///tmp/buildkit/buildkitd.sock")
        );
        assert_eq!(env("SNAPSHOTTER").as_deref(), Some("overlayfs"));
        assert_eq!(env("XDG_RUNTIME_DIR").as_deref(), Some("/tmp/buildkit/run"));
        assert_eq!(env("CACHE_DIRS").as_deref(), Some("/c/deps"));
        assert_eq!(env("RUNC").as_deref(), Some(crate::jobs::RUNC));
        assert_eq!(
            env("RUN_TARGETS").as_deref(),
            Some("/root/a \"b\"\n/buildit")
        );
        let listed = env("RUN_MOUNTS").unwrap();
        let listed: serde_json::Value =
            serde_json::from_str(&format!("[{}]", listed.strip_suffix(',').unwrap())).unwrap();
        assert_eq!(
            listed,
            serde_json::json!([
                { "destination": "/root/a \"b\"", "type": "bind", "source": "/c/deps",
                  "options": ["rbind", "ro"] },
                { "destination": "/buildit", "type": "bind", "source": "/buildit-out/facts",
                  "options": ["rbind", "rw"] },
            ])
        );
        for buildah in [
            "BUILDAH_ISOLATION",
            "CONTAINERS_STORAGE_CONF",
            "STORAGE_DRIVER",
        ] {
            assert_eq!(env(buildah), None);
        }
        let mounted: Vec<(&str, bool)> = builder["volumeMounts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                (
                    m["mountPath"].as_str().unwrap(),
                    m["readOnly"].as_bool().unwrap_or(false),
                )
            })
            .collect();
        assert_eq!(
            mounted,
            [
                ("/workspace", false),
                ("/buildit-out", false),
                ("/home/build/.local/share/containers", false),
                ("/buildit-inputs", true),
                ("/buildit-tools", true),
                ("/buildit-cluster", true),
            ]
        );

        let plain = buildkit_job(
            Registry::Insecure {
                host: "r:5000".to_string(),
            },
            StorageDriver::Vfs,
        );
        let builder = &plain["spec"]["template"]["spec"]["containers"][0];
        let snapshotter = builder["env"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "SNAPSHOTTER")
            .unwrap();
        assert_eq!(snapshotter["value"], "native");
        assert!(
            !builder["volumeMounts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["name"] == "cluster")
        );
    }

    /// [`crate::jobs::RUNC`] on a config as `buildkitd` writes one, with the paths it reads moved
    /// under `dir` and `runtime` for the runtime.
    #[cfg(target_os = "linux")]
    fn runc(
        dir: &std::path::Path,
        runtime: &str,
        config: &str,
        mounts: &[RunMount],
    ) -> (String, String, Option<i32>) {
        let bundle = dir.join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("config.json"), config).unwrap();
        let fragment = dir.join("mounts.json");
        std::fs::write(&fragment, crate::jobs::run_mounts_json(mounts)).unwrap();
        let targets = dir.join("targets");
        let listed: Vec<&str> = mounts.iter().map(|m| m.target.as_str()).collect();
        std::fs::write(&targets, format!("{}\n", listed.join("\n"))).unwrap();
        let script = crate::jobs::RUNC
            .replace("/tmp/buildkit/mounts.json", fragment.to_str().unwrap())
            .replace("/tmp/buildkit/targets", targets.to_str().unwrap())
            .replace("/buildit-tools/buildkit-runc", runtime);
        let out = std::process::Command::new("bash")
            .args(["-c", &script, "runc", "--log", "l", "run", "--bundle"])
            .arg(&bundle)
            .arg("id-1")
            .output()
            .unwrap();
        (
            std::fs::read_to_string(bundle.join("config.json")).unwrap(),
            String::from_utf8(out.stdout).unwrap(),
            out.status.code(),
        )
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_buildkit_runtime_drops_the_hostname_binds_the_pods_pts_and_adds_the_run_mounts() {
        let dir = tempfile::tempdir().unwrap();
        let config = concat!(
            r#"{"ociVersion":"1.2.0","process":{"args":["/bin/sh","-c","echo '\"mounts\":[' '\"hostname\":\"x\",' a&b"],"#,
            r#""env":["PATH=/bin"],"cwd":"/"},"root":{"path":"rootfs"},"hostname":"buildkitsandbox","#,
            r#""mounts":[{"destination":"/proc","type":"bind","source":"/proc","options":["rbind"]},"#,
            r#"{"destination":"/dev/pts","type":"devpts","source":"devpts","options":["nosuid","noexec","newinstance","ptmxmode=0666","mode=0620","gid=5"]},"#,
            r#"{"destination":"/dev/shm","type":"tmpfs","source":"shm","options":["nosuid"]}],"#,
            r#""linux":{"namespaces":[{"type":"uts"},{"type":"mount"}]}}"#,
        );
        let mounts = [
            RunMount {
                source: "/c/a&b".to_string(),
                target: "/root/it's \"here\"\\n".to_string(),
                read_only: true,
            },
            RunMount {
                source: "/buildit-out/facts".to_string(),
                target: "/buildit".to_string(),
                read_only: false,
            },
        ];
        let (edited, ran, code) = runc(dir.path(), "echo", config, &mounts);
        assert_eq!(code, Some(0));
        assert_eq!(
            ran.trim_end(),
            format!(
                "--log l run --bundle {} id-1",
                dir.path().join("bundle").display()
            ),
            "the runtime gets the arguments unchanged"
        );
        let edited: serde_json::Value = serde_json::from_str(&edited).unwrap();
        let original: serde_json::Value = serde_json::from_str(config).unwrap();
        assert!(edited.get("hostname").is_none(), "{edited}");
        assert_eq!(edited["process"], original["process"]);
        assert_eq!(edited["linux"], original["linux"]);
        assert_eq!(
            edited["mounts"],
            serde_json::json!([
                { "destination": "/root/it's \"here\"\\n", "type": "bind", "source": "/c/a&b",
                  "options": ["rbind", "ro"] },
                { "destination": "/buildit", "type": "bind", "source": "/buildit-out/facts",
                  "options": ["rbind", "rw"] },
                original["mounts"][0],
                { "destination": "/dev/pts", "type": "bind", "source": "/dev/pts",
                  "options": ["rbind", "nosuid", "noexec"] },
                original["mounts"][2],
            ])
        );

        let other = tempfile::tempdir().unwrap();
        let out = std::process::Command::new("bash")
            .args([
                "-c",
                &crate::jobs::RUNC.replace("/buildit-tools/buildkit-runc", "echo"),
                "runc",
                "kill",
                "id-1",
                "KILL",
            ])
            .current_dir(other.path())
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            "kill id-1 KILL\n",
            "a command with no bundle goes straight to the runtime"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_buildkit_runtime_leaves_no_mount_point_it_made_in_the_steps_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("bundle/rootfs");
        for there in ["etc", "opt/kept", "srv"] {
            std::fs::create_dir_all(root.join(there)).unwrap();
        }
        let fake = dir.path().join("fake-runc");
        std::fs::write(
            &fake,
            "#!/bin/sh\nr=$5/rootfs\n\
             mkdir -p $r/buildit $r/root/.cache/ccache $r/var/lib/deps $r/srv/used/cache\n\
             : > $r/etc/ccache.conf\necho left > $r/srv/used/file\nexit 7\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let mounts: Vec<RunMount> = [
            "/buildit",
            "/etc/ccache.conf",
            "/root/.cache/ccache",
            "/var/lib/deps",
            "/opt/kept",
            "/srv/used/cache",
        ]
        .iter()
        .map(|target| RunMount {
            source: "/s".to_string(),
            target: target.to_string(),
            read_only: false,
        })
        .collect();
        let config = r#"{"process":{"args":["true"]},"hostname":"h","mounts":[]}"#;
        let (_, _, code) = runc(dir.path(), fake.to_str().unwrap(), config, &mounts);
        assert_eq!(code, Some(7), "the step's exit code");
        for gone in [
            "buildit",
            "etc/ccache.conf",
            "root",
            "var",
            "srv/used/cache",
        ] {
            assert!(!root.join(gone).exists(), "{gone} is left");
        }
        for kept in ["etc", "opt/kept", "srv/used/file"] {
            assert!(root.join(kept).exists(), "{kept} is gone");
        }
    }

    #[test]
    fn a_layer_build_runs_its_script_in_the_base_and_pushes_from_the_archive() {
        let labels = labels();
        let env = BTreeMap::from([("MAX_JOBS".to_string(), "64".to_string())]);
        let caches = [
            ("deps".to_string(), "/src/app/.deps".to_string()),
            ("ccache".to_string(), "/root/.cache/ccache".to_string()),
        ];
        let job = serde_json::to_value(layer_job(
            &site(Some("builds"), cluster()),
            &ident("buildit-00000000000000a1", &labels),
            &LayerJob {
                context_digest: "sha256:ab",
                base: "docker.io/acme/base@sha256:cd",
                script: "make",
                path: "/src/app",
                env: &env,
                caches: &caches,
                ccache: true,
                limit_secs: 600,
            },
        ))
        .unwrap();
        assert_eq!(job["metadata"]["labels"]["buildit.dev/recipe"], "layer");
        assert_eq!(job["metadata"]["labels"]["buildit.dev/kind"], "build");
        let pod = &job["spec"]["template"]["spec"];
        let names: Vec<&str> = pod["initContainers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["fetch", "prepare", "archive"]);
        let main = &pod["containers"][0];
        assert_eq!(main["name"], "layer");
        assert_eq!(main["image"], "docker.io/acme/base@sha256:cd");
        let var = |c: &serde_json::Value, name: &str| {
            c["env"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["name"] == name)
                .map(|e| e["value"].as_str().unwrap().to_string())
        };
        assert_eq!(var(main, "BUILDIT_SCRIPT").as_deref(), Some("make"));
        assert_eq!(var(main, "MAX_JOBS").as_deref(), Some("64"));
        assert_eq!(
            var(main, "BUILDIT_EXCLUDE").as_deref(),
            Some("src/app/.deps"),
            "a cache under path stays out of the layer"
        );
        assert_eq!(
            var(main, "CCACHE_CONFIGPATH").as_deref(),
            Some("/buildit/ccache.conf")
        );
        let mounts = main["volumeMounts"].as_array().unwrap();
        assert!(mounts.iter().all(|m| {
            !["inputs", "archive-auth", "cluster"].contains(&m["name"].as_str().unwrap())
        }));
        let deps = mounts
            .iter()
            .find(|m| m["mountPath"] == "/src/app/.deps")
            .unwrap();
        assert_eq!(deps["name"], "caches");
        assert_eq!(deps["subPath"], "deps");
        assert_eq!(main["command"].as_array().unwrap().len(), 7);
        assert_eq!(main["command"][6], "/src/app");
        let archive = &pod["initContainers"][2];
        assert_eq!(
            var(archive, "BASE").as_deref(),
            Some("docker.io/acme/base@sha256:cd")
        );
        assert_eq!(
            var(archive, "DEST").as_deref(),
            Some("registry.builds.svc:5000/builds/buildit:sb-a-buildit-00000000000000a1")
        );
        assert_eq!(
            var(archive, "PLATFORM").as_deref(),
            Some("--platform=linux/amd64")
        );
        assert!(
            archive["volumeMounts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["name"] == "cluster"),
            "the archive alone pushes"
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
