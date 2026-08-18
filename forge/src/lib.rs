//! forge: engine-side workspace build + deploy, domain-agnostic.
//!
//! Build a source tree into an OCI image with **buildah** (rootless, daemonless, isolated storage),
//! push it with a mounted robot authfile, and roll a live Kubernetes deployment onto the candidate
//! through the typed [`kube`] client. Nothing here knows about any specific domain: the registry,
//! Dockerfile, push cred, and deploy target are all config ([`BuildConfig`] / [`DeployConfig`],
//! from `FORGE_*` env or passed explicitly). The loop pod is the one trusted place holding the push
//! cred; the agent only ever *asks* for a build via the domain's broker.
//!
//! ```text
//!   workspace ──buildah bud──▶ image ──push (authfile)──▶ registry:tag
//!                                                              │ kube set image + rollout
//!                                                              ▼
//!                                                         live deployment
//! ```

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A `buildah` invocation that ran and exited nonzero, with the log tail as evidence.
#[derive(Debug, thiserror::Error)]
pub enum BuildahFailed {
    #[error("buildah {args} failed while pruning build storage: {log}")]
    Prune { args: String, log: String },
    #[error("push failed for {image_ref}: {log}")]
    Push { image_ref: String, log: String },
}

/// Native-OCI image derivation (no container runtime): append a file-overlay layer onto a base image and
/// push it. The buildah-free path for "base image + a couple edited files" candidates.
pub mod oci;

/// Pure filesystem-diff <-> OCI-layer plumbing: entries -> a valid layer tar (with whiteouts) and,
/// inversely, apply a layer tar over a tree. No overlayfs / registry knowledge, so it's unit-testable.
pub mod layer;

/// Layer capture: run a command against a base image's own rootfs (unpack-once LRU cache + a swappable
/// executor, unprivileged user-namespace by default) and turn the files it writes into a pushed OCI
/// layer. The buildah-free fast path for "install this source tree into the image" candidates.
pub mod capture;

/// Typed Kubernetes operations via `kube-rs`: set image / rollout / exec / GPU headroom /
/// server-side apply, so the engine never shells `kubectl`.
pub mod kube;

/// The fleet file: the `[clusters.<name>]` spoke entries a hub delegates work to, their
/// reachability tiers, and the profile-side load/merge.
pub mod fleet;

/// Declarative image-build schema: the `[build.<name>]` manifest types, validation, the
/// closed-vocabulary template expander, and the watched-path content digest.
pub mod spec;

/// Append-only NDJSON ledger mechanics (flock-guarded append, torn-tail-tolerant fold,
/// quarantine); domain types stay with their owners.
pub mod ndjson;

/// Durable tool steps: a content-keyed ledger so a restarted process replays finished
/// work instead of repaying it.
pub mod steps;

/// The declarative-build backends: the detached rootless-buildah cluster Job and the GithubActions
/// dispatch, sharing one "build + push, crucible pins the digest" contract.
pub mod build;

/// Kueue-admitted GPU measure jobs: render + drive a `batch/v1` Job that runs a digest-pinned candidate
/// on the GPU (benchmark / lm_eval / profile) to a terminal state and collects its logs.
pub mod measure_job;

/// The GithubActions build backend: the thin GitHub REST client that dispatches a
/// `workflow_dispatch`, correlates + polls the resulting run, and validates the manifest's declared
/// input mapping against the workflow's introspected inputs.
pub mod github;

/// How to build + push a candidate image. `registry`/`authfile` are mandatory (a build can't proceed
/// without a destination and a credential); the rest carry domain-neutral defaults.
#[derive(Debug, Clone)]
pub struct BuildConfig {
    /// Destination repo *without* a tag, e.g. `quay.io/acme/candidate`.
    pub registry: String,
    /// Dockerfile relative to the build context.
    pub dockerfile: String,
    /// Robot push credential (an `auths` json scoped to the destination repo) mounted on the loop pod.
    pub authfile: PathBuf,
    /// Build platform, e.g. `linux/amd64`.
    pub platform: String,
    /// Isolated buildah storage so a build can't disturb a co-located podman's storage.
    /// `Some(dir)` ⇒ `--root <dir>/storage --runroot <dir>/runroot`; `None` ⇒ system storage.
    pub storage_root: Option<PathBuf>,
}

/// Which live deployment a candidate image is rolled onto.
#[derive(Debug, Clone)]
pub struct DeployConfig {
    pub namespace: String,
    pub deployment: String,
    pub container: String,
    /// Rollout-wait timeout in jiff's friendly format, e.g. `300s`.
    pub rollout_timeout: String,
}

/// The result of a build. A non-zero `buildah bud` is the caller's (agent's) problem (a compile
/// error or a bad Dockerfile edit) and the log is handed back to fix within the turn; infra
/// failures (no buildah, a push that can't auth) surface as `Err` instead.
/// Serialized as the recorded value of a build [`steps`] step, so the tagged shape is on disk.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BuildOutcome {
    /// Built and (after `build_and_push`) pushed; `image_ref` is live in the registry.
    Built { image_ref: String },
    /// The build failed; `log` is the tail of buildah's combined output.
    CompileError { log: String },
}

const DEFAULT_DOCKERFILE: &str = "Dockerfile";
const DEFAULT_PLATFORM: &str = "linux/amd64";
const DEFAULT_STORAGE_ROOT: &str = "/var/lib/forge";
const DEFAULT_ROLLOUT_TIMEOUT: &str = "300s";

/// Default location of the recorded latest-candidate ref (so `deploy` and an ensure-deployed
/// backstop find it without the caller threading it through).
pub const DEFAULT_LATEST_FILE: &str = "/var/lib/forge/latest-candidate";

/// Keep only the tail of a build log: the compiler error is at the bottom, the context-copy and
/// layer-cache preamble is noise.
const LOG_TAIL_LINES: usize = 200;

/// The forge storage root (`FORGE_STORAGE_ROOT`, default `/var/lib/forge`): the loop-pod volume
/// shared by the build staging, the recorded latest candidate, and the crucible↔broker handoff
/// files (`turn-token`, `wrap-up`).
pub fn storage_root() -> PathBuf {
    PathBuf::from(env_or("FORGE_STORAGE_ROOT", DEFAULT_STORAGE_ROOT))
}

impl BuildConfig {
    /// Read from `FORGE_REGISTRY` / `FORGE_AUTHFILE` (required) + `FORGE_DOCKERFILE` /
    /// `FORGE_PLATFORM` / `FORGE_STORAGE_ROOT` (defaulted). Errors if a required var is unset.
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            registry: env_req("FORGE_REGISTRY")?,
            dockerfile: env_or("FORGE_DOCKERFILE", DEFAULT_DOCKERFILE),
            authfile: PathBuf::from(env_req("FORGE_AUTHFILE")?),
            platform: env_or("FORGE_PLATFORM", DEFAULT_PLATFORM),
            storage_root: Some(storage_root()),
        })
    }

    /// The full `registry:tag` image ref for a tag.
    pub fn image_ref(&self, tag: &str) -> String {
        format!("{}:{}", self.registry, tag)
    }

    /// Buildah's global storage-isolation flags (before the subcommand); empty on system storage.
    fn storage_flags(&self) -> Vec<String> {
        match &self.storage_root {
            Some(root) => vec![
                "--root".into(),
                root.join("storage").to_string_lossy().into_owned(),
                "--runroot".into(),
                root.join("runroot").to_string_lossy().into_owned(),
            ],
            None => Vec::new(),
        }
    }
}

impl DeployConfig {
    /// Read from `FORGE_DEPLOY_NAMESPACE` / `FORGE_DEPLOY_NAME` / `FORGE_DEPLOY_CONTAINER` (required)
    /// + `FORGE_DEPLOY_TIMEOUT` (defaulted).
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            namespace: env_req("FORGE_DEPLOY_NAMESPACE")?,
            deployment: env_req("FORGE_DEPLOY_NAME")?,
            container: env_req("FORGE_DEPLOY_CONTAINER")?,
            rollout_timeout: env_or("FORGE_DEPLOY_TIMEOUT", DEFAULT_ROLLOUT_TIMEOUT),
        })
    }
}

/// `buildah bud` argv (after the `buildah` program name): storage isolation, then build from
/// `context_dir`/`dockerfile` to `image_ref`. `COMMIT_SHA` carries the tag so the built artifact can
/// report what it was built from.
fn buildah_bud_args(cfg: &BuildConfig, context_dir: &Path, image_ref: &str) -> Vec<String> {
    let mut a = cfg.storage_flags();
    a.extend([
        "bud".into(),
        "--platform".into(),
        cfg.platform.clone(),
        "--file".into(),
        cfg.dockerfile.clone(),
        "--build-arg".into(),
        format!("COMMIT_SHA={}", crate::oci::tag_of(image_ref)),
        "--tag".into(),
        image_ref.to_string(),
        context_dir.to_string_lossy().into_owned(),
    ]);
    a
}

/// `buildah push` argv: push `image_ref` with the robot authfile, from the same isolated storage.
fn buildah_push_args(cfg: &BuildConfig, image_ref: &str) -> Vec<String> {
    let mut a = cfg.storage_flags();
    a.extend([
        "push".into(),
        "--authfile".into(),
        cfg.authfile.to_string_lossy().into_owned(),
        image_ref.to_string(),
    ]);
    a
}

/// `buildah rm --all` argv: drop every working container in the isolated storage.
fn buildah_rm_all_args(cfg: &BuildConfig) -> Vec<String> {
    let mut a = cfg.storage_flags();
    a.extend(["rm".into(), "--all".into()]);
    a
}

/// `buildah rmi --all --force` argv: drop every image (base layers included) in the isolated storage.
fn buildah_rmi_all_args(cfg: &BuildConfig) -> Vec<String> {
    let mut a = cfg.storage_flags();
    a.extend(["rmi".into(), "--all".into(), "--force".into()]);
    a
}

/// `buildah rmi <ref>` argv: drop one image by ref.
fn buildah_rmi_args(cfg: &BuildConfig, image_ref: &str) -> Vec<String> {
    let mut a = cfg.storage_flags();
    a.extend(["rmi".into(), image_ref.to_string()]);
    a
}

/// `buildah rmi --prune` argv: drop dangling (untagged) images only; tagged bases survive.
fn buildah_rmi_prune_args(cfg: &BuildConfig) -> Vec<String> {
    let mut a = cfg.storage_flags();
    a.extend(["rmi".into(), "--prune".into()]);
    a
}

/// Empty the ISOLATED buildah storage (containers, then images). The vfs storage under a per-broker
/// `storage_root` is scratch rather than a cache: every build leaves its full layer set behind, and
/// nothing else reads it, so leaving it accumulates disk until the volume dies. A `None`
/// storage_root is a hard no-op: `--all` against SYSTEM storage could take out a co-located
/// engine's containers.
pub fn prune_storage(cfg: &BuildConfig) -> Result<()> {
    if cfg.storage_root.is_none() {
        return Ok(());
    }
    for args in [buildah_rm_all_args(cfg), buildah_rmi_all_args(cfg)] {
        prune_step(&args)?;
    }
    Ok(())
}

/// [`prune_storage`], but the pulled BASE image survives: drop the working containers, the built
/// candidate (`built_ref`, already pushed — the registry and the memo own it now), and dangling
/// intermediates. Re-pulling the multi-GB base cost ~10 minutes of every build; a tagged base in
/// the isolated storage is the one layer set worth keeping between builds.
pub fn prune_storage_keep_base(cfg: &BuildConfig, built_ref: Option<&str>) -> Result<()> {
    if cfg.storage_root.is_none() {
        return Ok(());
    }
    prune_step(&buildah_rm_all_args(cfg))?;
    if let Some(built) = built_ref {
        prune_step(&buildah_rmi_args(cfg, built))?;
    }
    prune_step(&buildah_rmi_prune_args(cfg))
}

fn prune_step(args: &[String]) -> Result<()> {
    let out = run("buildah", args, None)
        .context("running buildah to prune the isolated build storage")?;
    if !out.status.success() {
        return Err(BuildahFailed::Prune {
            args: args.join(" "),
            log: log_tail(&out),
        }
        .into());
    }
    Ok(())
}

/// Parse a `kubectl`-style rollout timeout (`300s`, `10m`, `1h`) into a [`Duration`] via jiff's friendly
/// format. Falls back to 300s on anything unparseable so a typo never makes the wait infinite.
fn parse_timeout(s: &str) -> std::time::Duration {
    s.trim()
        .parse::<jiff::SignedDuration>()
        .ok()
        .and_then(|d| std::time::Duration::try_from(d).ok())
        .unwrap_or(std::time::Duration::from_secs(300))
}

/// Build the candidate (no push). On a non-zero `buildah bud`, returns
/// [`BuildOutcome::CompileError`] with the log tail; reserves `Err` for "couldn't run buildah".
fn build(cfg: &BuildConfig, context_dir: &Path, tag: &str) -> Result<BuildOutcome> {
    let image_ref = cfg.image_ref(tag);
    ensure_storage_dirs(cfg)?;
    eprintln!("==> buildah bud {} -> {image_ref}", context_dir.display());
    let out = run(
        "buildah",
        &buildah_bud_args(cfg, context_dir, &image_ref),
        None,
    )
    .context("running `buildah bud` (is buildah on PATH?)")?;
    if out.status.success() {
        Ok(BuildOutcome::Built { image_ref })
    } else {
        Ok(BuildOutcome::CompileError {
            log: log_tail(&out),
        })
    }
}

/// Build then, if it built, push. Recording the latest-candidate is the caller's job (so a failed
/// push never advertises a candidate that isn't in the registry).
pub fn build_and_push(cfg: &BuildConfig, context_dir: &Path, tag: &str) -> Result<BuildOutcome> {
    match build(cfg, context_dir, tag)? {
        BuildOutcome::Built { image_ref } => {
            eprintln!("==> buildah push {image_ref}");
            let out = run("buildah", &buildah_push_args(cfg, &image_ref), None)
                .context("running `buildah push`")?;
            if !out.status.success() {
                return Err(BuildahFailed::Push {
                    image_ref,
                    log: log_tail(&out),
                }
                .into());
            }
            Ok(BuildOutcome::Built { image_ref })
        }
        compile_err => Ok(compile_err),
    }
}

/// [`build`], but buildah's combined output streams WRITE-THROUGH into `log`: the child gets the
/// file descriptors, so bytes land on disk as buildah prints them and a concurrent reader can tail
/// the file mid-build. On a compile error the returned log tail is read back from the same file.
fn build_streaming(
    cfg: &BuildConfig,
    context_dir: &Path,
    tag: &str,
    log: &Path,
) -> Result<BuildOutcome> {
    let image_ref = cfg.image_ref(tag);
    ensure_storage_dirs(cfg)?;
    eprintln!(
        "==> buildah bud {} -> {image_ref} (live log: {})",
        context_dir.display(),
        log.display()
    );
    let status = run_streaming_traced(
        "buildah",
        &buildah_bud_args(cfg, context_dir, &image_ref),
        log,
    )
    .context("running `buildah bud` (is buildah on PATH?)")?;
    if status.success() {
        Ok(BuildOutcome::Built { image_ref })
    } else {
        Ok(BuildOutcome::CompileError {
            log: file_log_tail(log),
        })
    }
}

/// [`build_and_push`] with a live log file: the push output appends to the same `log`.
pub fn build_and_push_streaming(
    cfg: &BuildConfig,
    context_dir: &Path,
    tag: &str,
    log: &Path,
) -> Result<BuildOutcome> {
    match build_streaming(cfg, context_dir, tag, log)? {
        BuildOutcome::Built { image_ref } => {
            eprintln!("==> buildah push {image_ref}");
            let status = run_streaming("buildah", &buildah_push_args(cfg, &image_ref), log)
                .context("running `buildah push`")?;
            if !status.success() {
                return Err(BuildahFailed::Push {
                    image_ref,
                    log: file_log_tail(log),
                }
                .into());
            }
            Ok(BuildOutcome::Built { image_ref })
        }
        compile_err => Ok(compile_err),
    }
}

/// [`run_streaming`], but the child's stdout is teed through this process so buildah's
/// `STEP n/m:` markers become child tracing spans — a 30-minute build shows up in a trace as
/// per-layer bars instead of one opaque block. Bytes still land in `log` as they arrive (stdout
/// line-at-a-time, stderr in raw chunks), so a concurrent `fetch_log` tail behaves as before.
fn run_streaming_traced(
    program: &str,
    args: &[String],
    log: &Path,
) -> Result<std::process::ExitStatus> {
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("opening live log {}", log.display()))?;
    let file = Arc::new(Mutex::new(file));

    let mut child = Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning `{program}`"))?;

    // The scanning thread has no span context of its own; capture the caller's (the broker's
    // tool-call span) so step spans parent onto the build instead of forming orphan traces.
    let build_span = tracing::Span::current();

    let stderr_file = Arc::clone(&file);
    let stderr_thread = child.stderr.take().map(|mut err| {
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = err.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if let Ok(mut f) = stderr_file.lock() {
                    let _ = f.write_all(&buf[..n]);
                }
            }
        })
    });

    let stdout_thread = child.stdout.take().map(|mut out| {
        let file = Arc::clone(&file);
        std::thread::spawn(move || {
            // Raw chunks, never `read_line`. A build's stdout is arbitrary bytes — a latin-1 path
            // in vendored source, nvcc's ANSI noise — and `read_line` returns InvalidData on the
            // first non-UTF-8 one. Breaking there drops the reader, closes the pipe, and the Go
            // child takes SIGPIPE on its next write: one stray byte killed a 30-minute build.
            // Chunks also keep the log honest for `\r`-updated progress bars, which carry no
            // newline for minutes and so never reached the file (or a live `fetch_log` tail).
            let mut buf = [0u8; 8192];
            // Only the trailing partial line is held, for marker scanning. Capped so a child that
            // never emits a terminator cannot grow it without bound.
            let mut pending: Vec<u8> = Vec::new();
            // Holding the previous step's span until the next marker makes its close time the
            // next step's start, so span duration is the layer's real wall-clock.
            let mut step_span: Option<tracing::Span> = None;
            let on_line = |bytes: &[u8], step_span: &mut Option<tracing::Span>| {
                // Lossy: marker parsing must never be what decides a build lives or dies.
                let text = String::from_utf8_lossy(bytes);
                if let Some((step, total, instruction)) = parse_step_marker(text.trim_end()) {
                    drop(step_span.take());
                    *step_span = Some(tracing::info_span!(
                        parent: &build_span,
                        "buildah.step",
                        step,
                        total,
                        instruction,
                    ));
                }
            };
            loop {
                let n = match out.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    // A real read error ends the tee, but the child keeps its own stderr and exit
                    // status; the caller still reports honestly.
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                let chunk = &buf[..n];
                if let Ok(mut f) = file.lock() {
                    let _ = f.write_all(chunk);
                }
                for &b in chunk {
                    if b == b'\n' || b == b'\r' {
                        on_line(&pending, &mut step_span);
                        pending.clear();
                    } else if pending.len() < 64 * 1024 {
                        pending.push(b);
                    }
                }
            }
            if !pending.is_empty() {
                on_line(&pending, &mut step_span);
            }
            drop(step_span);
        })
    });

    let status = child
        .wait()
        .with_context(|| format!("waiting for `{program}`"))?;
    if let Some(t) = stdout_thread {
        let _ = t.join();
    }
    if let Some(t) = stderr_thread {
        let _ = t.join();
    }
    Ok(status)
}

/// Parse a buildah `STEP <n>/<m>: <instruction>` marker; the instruction is clipped so a huge RUN
/// body doesn't become a span attribute.
fn parse_step_marker(line: &str) -> Option<(u32, u32, String)> {
    let rest = line.strip_prefix("STEP ")?;
    let (counts, instruction) = rest.split_once(": ")?;
    let (step, total) = counts.split_once('/')?;
    let instruction: String = instruction.chars().take(120).collect();
    Some((step.parse().ok()?, total.parse().ok()?, instruction))
}

/// Run a command with stdout AND stderr appended straight into `log`. The descriptors go to the
/// child, so there is no buffer-then-write step in this process at all.
fn run_streaming(program: &str, args: &[String], log: &Path) -> Result<std::process::ExitStatus> {
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("opening live log {}", log.display()))?;
    let err = out
        .try_clone()
        .with_context(|| format!("duplicating live log fd for {}", log.display()))?;
    Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::from(out))
        .stderr(std::process::Stdio::from(err))
        .status()
        .with_context(|| format!("spawning `{program}`"))
}

/// The last [`LOG_TAIL_LINES`] of a streamed log file (lossy; a build log is for humans).
fn file_log_tail(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap_or_default();
    let s = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(LOG_TAIL_LINES);
    lines[start..].join("\n")
}

/// Point the live deployment at `image_ref` and wait for the rollout, via the typed kube client (no
/// `kubectl`). The rollout restart re-pulls even when the tag string is unchanged (mutable tags).
pub fn deploy(cfg: &DeployConfig, image_ref: &str) -> Result<()> {
    eprintln!("==> set image {} -> {image_ref}", cfg.deployment);
    crate::kube::set_image(&cfg.namespace, &cfg.deployment, &cfg.container, image_ref)?;
    crate::kube::rollout_restart(&cfg.namespace, &cfg.deployment)?;
    crate::kube::rollout_status(
        &cfg.namespace,
        &cfg.deployment,
        parse_timeout(&cfg.rollout_timeout),
    )?;
    eprintln!("==> deployment ready on {image_ref}");
    Ok(())
}

/// The container's currently-deployed image, or `None` if the deployment/container isn't found.
pub fn current_image(cfg: &DeployConfig) -> Result<Option<String>> {
    crate::kube::current_image(&cfg.namespace, &cfg.deployment, &cfg.container)
}

/// Record the latest built+pushed image ref (atomically) so deploy/ensure-deployed can find it.
pub fn record_latest(path: &Path, image_ref: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("{image_ref}\n")).with_context(|| format!("writing {tmp:?}"))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Read the recorded latest candidate, or `None` if nothing has been built yet.
pub fn read_latest(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let t = s.trim().to_string();
            Ok((!t.is_empty()).then_some(t))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// A short, content-ish tag for a build context: the git short SHA of `dir` when it's a checkout,
/// else `wip`. So re-pushes stay distinguishable without the caller inventing a tag.
pub fn context_tag(dir: &Path) -> String {
    git2::Repository::open(dir)
        .and_then(|repo| {
            let head = repo.head()?.peel_to_commit()?;
            Ok(head
                .as_object()
                .short_id()?
                .as_str()
                .unwrap_or("wip")
                .to_string())
        })
        .unwrap_or_else(|_: git2::Error| "wip".to_string())
}

/// Pre-create the isolated buildah storage dirs so the first build doesn't trip over a missing root.
fn ensure_storage_dirs(cfg: &BuildConfig) -> Result<()> {
    if let Some(root) = &cfg.storage_root {
        for sub in ["storage", "runroot"] {
            let d = root.join(sub);
            std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
        }
    }
    Ok(())
}

/// Run a command capturing both streams. Errors only when the process can't be spawned (the caller
/// decides what a non-zero exit means).
fn run(program: &str, args: &[String], cwd: Option<&Path>) -> Result<Output> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.output()
        .with_context(|| format!("spawning `{program}`"))
}

/// The tail of a command's combined stdout+stderr, for handing a build failure back to the caller.
fn log_tail(out: &Output) -> String {
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    let lines: Vec<&str> = combined.lines().collect();
    let start = lines.len().saturating_sub(LOG_TAIL_LINES);
    lines[start..].join("\n")
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_req(key: &str) -> Result<String> {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .with_context(|| format!("{key} must be set"))
}

#[cfg(test)]
mod tests {
    use crate::*;

    /// The keep-base prune must never touch SYSTEM storage, and its argv set drops containers,
    /// the built candidate, and dangling images — never `--all` images (that is what re-pulled
    /// the base every build).
    #[test]
    fn keep_base_prune_args_spare_tagged_images() {
        let cfg = build_cfg();
        assert_eq!(
            buildah_rmi_args(&cfg, "quay.io/acme/candidate:codegen-1")[cfg.storage_flags().len()..],
            [
                "rmi".to_string(),
                "quay.io/acme/candidate:codegen-1".to_string()
            ]
        );
        assert_eq!(
            buildah_rmi_prune_args(&cfg)[cfg.storage_flags().len()..],
            ["rmi".to_string(), "--prune".to_string()]
        );
        let mut no_root = build_cfg();
        no_root.storage_root = None;
        assert!(prune_storage_keep_base(&no_root, Some("x")).is_ok());
    }

    #[test]
    fn step_markers_parse_and_ignore_noise() {
        assert_eq!(
            parse_step_marker("STEP 4/4: RUN MAX_JOBS=32 python3 setup.py build_ext"),
            Some((4, 4, "RUN MAX_JOBS=32 python3 setup.py build_ext".into()))
        );
        let long = format!("STEP 2/4: RUN {}", "x".repeat(500));
        let (_, _, instruction) = parse_step_marker(&long).unwrap();
        assert_eq!(instruction.chars().count(), 120);
        for noise in [
            "Copying blob sha256:abc",
            "STEP marker mentioned mid-log",
            "-- Configuring done (95.6s)",
            "STEP not/numeric: RUN true",
        ] {
            assert_eq!(parse_step_marker(noise), None, "{noise}");
        }
    }

    fn build_cfg() -> BuildConfig {
        BuildConfig {
            registry: "quay.io/acme/candidate".into(),
            dockerfile: "Dockerfile.candidate".into(),
            authfile: PathBuf::from("/etc/quay/candidate.json"),
            platform: "linux/amd64".into(),
            storage_root: Some(PathBuf::from("/var/lib/forge")),
        }
    }

    #[test]
    fn image_ref_joins_registry_and_tag() {
        assert_eq!(
            build_cfg().image_ref("abc123"),
            "quay.io/acme/candidate:abc123"
        );
    }

    #[test]
    fn bud_args_isolate_storage_and_carry_the_commit_arg() {
        let cfg = build_cfg();
        let img = cfg.image_ref("abc123");
        let a = buildah_bud_args(&cfg, Path::new("/tmp/build/src"), &img);
        let bud = a.iter().position(|s| s == "bud").expect("has bud");
        assert!(
            a[..bud]
                .windows(2)
                .any(|w| w[0] == "--root" && w[1] == "/var/lib/forge/storage")
        );
        assert!(
            a[..bud]
                .windows(2)
                .any(|w| w[0] == "--runroot" && w[1] == "/var/lib/forge/runroot")
        );
        assert!(a.contains(&"--platform".to_string()));
        assert!(a.contains(&"linux/amd64".to_string()));
        assert!(a.contains(&"COMMIT_SHA=abc123".to_string()));
        assert!(a.contains(&img));
        assert_eq!(
            a.last().unwrap(),
            "/tmp/build/src",
            "context is the last arg"
        );
    }

    #[test]
    fn bud_args_omit_storage_flags_on_system_storage() {
        let mut cfg = build_cfg();
        cfg.storage_root = None;
        let a = buildah_bud_args(&cfg, Path::new("/ctx"), "img:t");
        assert_eq!(a.first().unwrap(), "bud", "no global flags => bud is first");
        assert!(!a.contains(&"--root".to_string()));
    }

    #[test]
    fn run_streaming_writes_both_streams_through_to_the_file() {
        let dir = std::env::temp_dir().join(format!("forge-stream-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("live.log");
        let status = run_streaming(
            "sh",
            &[
                "-c".into(),
                "printf from-stdout; printf from-stderr >&2".into(),
            ],
            &log,
        )
        .unwrap();
        assert!(status.success());
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(body.contains("from-stdout"), "{body}");
        assert!(body.contains("from-stderr"), "{body}");
        // A second invocation APPENDS (the push step shares the bud step's log file).
        run_streaming("sh", &["-c".into(), "echo pushed".into()], &log).unwrap();
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(body.contains("from-stdout"), "{body}");
        assert!(body.contains("pushed"), "{body}");
        // A failing child reports its status without erroring the runner.
        let status = run_streaming("sh", &["-c".into(), "exit 7".into()], &log).unwrap();
        assert!(!status.success());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_log_tail_keeps_only_the_last_lines() {
        let dir = std::env::temp_dir().join(format!("forge-tail-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("big.log");
        let body: String = (0..LOG_TAIL_LINES + 50)
            .map(|i| format!("line {i}\n"))
            .collect();
        std::fs::write(&log, body).unwrap();
        let tail = file_log_tail(&log);
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), LOG_TAIL_LINES);
        assert_eq!(lines[0], "line 50");
        // A missing file degrades to an empty tail (the error path already has its own message).
        assert_eq!(file_log_tail(&dir.join("nope.log")), "");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prune_args_target_the_isolated_storage() {
        let cfg = build_cfg();
        let rm = buildah_rm_all_args(&cfg);
        assert_eq!(
            rm,
            [
                "--root",
                "/var/lib/forge/storage",
                "--runroot",
                "/var/lib/forge/runroot",
                "rm",
                "--all"
            ]
        );
        let rmi = buildah_rmi_all_args(&cfg);
        assert_eq!(&rmi[4..], ["rmi", "--all", "--force"]);
        assert!(rmi.contains(&"--root".to_string()));
    }

    #[test]
    fn prune_storage_is_a_hard_noop_on_system_storage() {
        let mut cfg = build_cfg();
        cfg.storage_root = None;
        // Must return Ok WITHOUT spawning buildah: `rm/rmi --all` against SYSTEM storage could take
        // out a co-located engine's containers. (Spawning would also fail on buildah-less hosts;
        // this test runs everywhere.)
        assert!(prune_storage(&cfg).is_ok());
    }

    #[test]
    fn push_args_use_the_authfile() {
        let a = buildah_push_args(&build_cfg(), "quay.io/acme/candidate:t");
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--authfile" && w[1] == "/etc/quay/candidate.json")
        );
        assert!(a.contains(&"quay.io/acme/candidate:t".to_string()));
    }

    #[test]
    fn parse_timeout_reads_friendly_durations() {
        assert_eq!(parse_timeout("300s"), std::time::Duration::from_secs(300));
        assert_eq!(parse_timeout("10m"), std::time::Duration::from_secs(600));
        assert_eq!(parse_timeout("1h"), std::time::Duration::from_secs(3600));
        // Garbage falls back to the 300s default (never an infinite wait).
        assert_eq!(
            parse_timeout("garbage"),
            std::time::Duration::from_secs(300)
        );
    }

    #[test]
    fn record_then_read_round_trips() {
        let dir = std::env::temp_dir().join(format!("forge-test-{}", std::process::id()));
        let f = dir.join("latest-candidate");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(read_latest(&f).unwrap(), None, "absent before any write");
        record_latest(&f, "quay.io/acme/candidate:abc").unwrap();
        assert_eq!(
            read_latest(&f).unwrap().as_deref(),
            Some("quay.io/acme/candidate:abc")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn traced_streaming_survives_non_utf8_and_keeps_reading() {
        // The regression this guards: `read_line` errors on the first non-UTF-8 byte, the reader
        // drops, the pipe closes, and the Go child dies of SIGPIPE mid-build. Everything after the
        // bad byte must still reach the log, and the child must exit 0.
        let dir = std::env::temp_dir().join(format!("forge-traced-utf8-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("live.log");
        let status = run_streaming_traced(
            "sh",
            &[
                "-c".into(),
                // a lone 0xFF, then a real STEP marker, then more output
                r"printf 'before\n'; printf '\377'; printf '\nSTEP 2/5: RUN make\nafter\n'".into(),
            ],
            &log,
        )
        .unwrap();
        assert!(status.success(), "child must not be killed: {status:?}");
        let body = std::fs::read(&log).unwrap();
        assert!(
            body.windows(6).any(|w| w == b"before"),
            "pre-bad-byte output lost"
        );
        assert!(
            body.contains(&0xFF),
            "the raw byte must pass through verbatim"
        );
        assert!(
            body.windows(5).any(|w| w == b"after"),
            "reading stopped at the bad byte"
        );
    }

    #[test]
    fn traced_streaming_flushes_carriage_return_progress_without_a_newline() {
        // `\r`-updated progress bars carry no newline for minutes. Under read_line they reached
        // neither the log nor a live `fetch_log` tail until a newline finally landed.
        let dir = std::env::temp_dir().join(format!("forge-traced-cr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("live.log");
        let status = run_streaming_traced(
            "sh",
            &["-c".into(), r"printf '10%%\r50%%\r100%%'".into()],
            &log,
        )
        .unwrap();
        assert!(status.success());
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(body.contains("10%") && body.contains("100%"), "{body}");
    }
}
